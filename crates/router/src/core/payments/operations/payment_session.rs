use std::collections::HashSet;

use api_models::payments::PaymentsSessionRequest;
use async_trait::async_trait;
use common_utils::ext_traits::ValueExt;
use error_stack::ResultExt;
use router_env::{instrument, tracing};

use super::{
    BoxedOperation, DeriveFlow, Domain, GetTracker, Operation, UpdateTracker, ValidateRequest,
};
use crate::{
    core::{
        errors::{self, RouterResult, StorageErrorExt},
        payments::{self, helpers, operations, PaymentData},
    },
    db::StorageInterface,
    pii,
    pii::Secret,
    routes::AppState,
    services,
    types::{
        self,
        api::{self, enums as api_enums, PaymentIdTypeExt},
        storage::{self, enums},
        transformers::ForeignInto,
    },
    utils::OptionExt,
};

#[derive(Debug, Clone, Copy)]
// #[operation(ops = "all", flow = "session")]
pub struct PaymentSession;
#[async_trait]
impl Operation<PaymentsSessionRequest> for &PaymentSession {
    fn to_validate_request(
        &self,
    ) -> RouterResult<&(dyn ValidateRequest<PaymentsSessionRequest> + Send + Sync)> {
        Ok(*self)
    }
    fn to_get_tracker(
        &self,
    ) -> RouterResult<&(dyn GetTracker<PaymentData, PaymentsSessionRequest> + Send + Sync)> {
        Ok(*self)
    }
    fn to_domain(&self) -> RouterResult<&(dyn Domain<PaymentsSessionRequest>)> {
        Ok(*self)
    }
    fn to_update_tracker(
        &self,
    ) -> RouterResult<&(dyn UpdateTracker<PaymentData, PaymentsSessionRequest> + Send + Sync)> {
        Ok(*self)
    }

    async fn calling_connector(
        &self,
        state: &AppState,
        merchant_account: &storage::MerchantAccount,
        payment_data: PaymentData,
        customer: &Option<storage_models::customers::Customer>,
        call_connector_action: payments::CallConnectorAction,
        connector_details: api::ConnectorCallType,
        validate_result: operations::ValidateResult<'_>,
    ) -> RouterResult<PaymentData> {
        self.call_connector(
            state,
            merchant_account,
            payment_data,
            customer,
            call_connector_action,
            connector_details,
            validate_result,
        )
        .await
    }
}
#[async_trait]
impl Operation<PaymentsSessionRequest> for PaymentSession {
    fn to_validate_request(
        &self,
    ) -> RouterResult<&(dyn ValidateRequest<PaymentsSessionRequest> + Send + Sync)> {
        Ok(self)
    }
    fn to_get_tracker(
        &self,
    ) -> RouterResult<&(dyn GetTracker<PaymentData, PaymentsSessionRequest> + Send + Sync)> {
        Ok(self)
    }
    fn to_domain(&self) -> RouterResult<&dyn Domain<PaymentsSessionRequest>> {
        Ok(self)
    }
    fn to_update_tracker(
        &self,
    ) -> RouterResult<&(dyn UpdateTracker<PaymentData, PaymentsSessionRequest> + Send + Sync)> {
        Ok(self)
    }

    async fn calling_connector(
        &self,
        state: &AppState,
        merchant_account: &storage::MerchantAccount,
        payment_data: PaymentData,
        customer: &Option<storage_models::customers::Customer>,
        call_connector_action: payments::CallConnectorAction,
        connector_details: api::ConnectorCallType,
        validate_result: operations::ValidateResult<'_>,
    ) -> RouterResult<PaymentData> {
        self.call_connector(
            state,
            merchant_account,
            payment_data,
            customer,
            call_connector_action,
            connector_details,
            validate_result,
        )
        .await
    }
}

#[async_trait]
impl GetTracker<PaymentData, PaymentsSessionRequest> for PaymentSession {
    #[instrument(skip_all)]
    async fn get_trackers<'a>(
        &'a self,
        state: &'a AppState,
        payment_id: &api::PaymentIdType,
        request: &PaymentsSessionRequest,
        _mandate_type: Option<api::MandateTxnType>,
        merchant_account: &storage::MerchantAccount,
    ) -> RouterResult<(
        BoxedOperation<'a, PaymentsSessionRequest>,
        PaymentData,
        Option<payments::CustomerDetails>,
    )> {
        let payment_id = payment_id
            .get_payment_intent_id()
            .change_context(errors::ApiErrorResponse::PaymentNotFound)?;

        let db = &*state.store;
        let merchant_id = &merchant_account.merchant_id;
        let storage_scheme = merchant_account.storage_scheme;

        let mut payment_attempt = db
            .find_payment_attempt_by_payment_id_merchant_id(
                &payment_id,
                merchant_id,
                storage_scheme,
            )
            .await
            .map_err(|error| {
                error.to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)
            })?;

        let mut payment_intent = db
            .find_payment_intent_by_payment_id_merchant_id(&payment_id, merchant_id, storage_scheme)
            .await
            .map_err(|error| {
                error.to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)
            })?;

        let currency = payment_intent.currency.get_required_value("currency")?;

        payment_attempt.payment_method = Some(enums::PaymentMethodType::Wallet);

        let amount = payment_intent.amount.into();

        helpers::authenticate_client_secret(
            Some(&request.client_secret),
            payment_intent.client_secret.as_ref(),
        )?;

        let shipping_address = helpers::get_address_for_payment_request(
            db,
            None,
            payment_intent.shipping_address_id.as_deref(),
            merchant_id,
            &payment_intent.customer_id,
        )
        .await?;

        let billing_address = helpers::get_address_for_payment_request(
            db,
            None,
            payment_intent.billing_address_id.as_deref(),
            merchant_id,
            &payment_intent.customer_id,
        )
        .await?;

        payment_intent.shipping_address_id = shipping_address.clone().map(|x| x.address_id);
        payment_intent.billing_address_id = billing_address.clone().map(|x| x.address_id);

        let connector_response = db
            .find_connector_response_by_payment_id_merchant_id_attempt_id(
                &payment_intent.payment_id,
                &payment_intent.merchant_id,
                &payment_attempt.attempt_id,
                storage_scheme,
            )
            .await
            .map_err(|error| {
                error
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                    .attach_printable("Database error when finding connector response")
            })?;

        let customer_details = payments::CustomerDetails {
            customer_id: payment_intent.customer_id.clone(),
            name: None,
            email: None,
            phone: None,
            phone_country_code: None,
        };

        Ok((
            Box::new(self),
            PaymentData {
                payment_intent,
                payment_attempt,
                currency,
                amount,
                email: None::<Secret<String, pii::Email>>,
                mandate_id: None,
                token: None,
                setup_mandate: None,
                address: payments::PaymentAddress {
                    shipping: shipping_address.as_ref().map(|a| a.foreign_into()),
                    billing: billing_address.as_ref().map(|a| a.foreign_into()),
                },
                confirm: None,
                payment_method_data: None,
                force_sync: None,
                refunds: vec![],
                sessions_token: vec![],
                connector_response,
                card_cvc: None,
            },
            Some(customer_details),
        ))
    }
}

#[async_trait]
impl UpdateTracker<PaymentData, PaymentsSessionRequest> for PaymentSession {
    #[instrument(skip_all)]
    async fn update_trackers<'b>(
        &'b self,
        db: &dyn StorageInterface,
        _payment_id: &api::PaymentIdType,
        mut payment_data: PaymentData,
        _customer: Option<storage::Customer>,
        storage_scheme: enums::MerchantStorageScheme,
    ) -> RouterResult<(BoxedOperation<'b, PaymentsSessionRequest>, PaymentData)> {
        let metadata = payment_data.payment_intent.metadata.clone();
        payment_data.payment_intent = match metadata {
            Some(metadata) => db
                .update_payment_intent(
                    payment_data.payment_intent,
                    storage::PaymentIntentUpdate::MetadataUpdate { metadata },
                    storage_scheme,
                )
                .await
                .map_err(|error| {
                    error.to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)
                })?,
            None => payment_data.payment_intent,
        };

        Ok((Box::new(self), payment_data))
    }
}

impl ValidateRequest<PaymentsSessionRequest> for PaymentSession {
    #[instrument(skip_all)]
    fn validate_request<'a, 'b>(
        &'b self,
        request: &PaymentsSessionRequest,
        merchant_account: &'a storage::MerchantAccount,
    ) -> RouterResult<(
        BoxedOperation<'b, PaymentsSessionRequest>,
        operations::ValidateResult<'a>,
    )> {
        //paymentid is already generated and should be sent in the request
        let given_payment_id = request.payment_id.clone();

        Ok((
            Box::new(self),
            operations::ValidateResult {
                merchant_id: &merchant_account.merchant_id,
                payment_id: api::PaymentIdType::PaymentIntentId(given_payment_id),
                mandate_type: None,
                storage_scheme: merchant_account.storage_scheme,
            },
        ))
    }
}

#[derive(serde::Deserialize, Default)]
pub struct PaymentMethodEnabled {
    payment_method: String,
}

#[async_trait]
impl<Op: Send + Sync + Operation<PaymentsSessionRequest>> Domain<PaymentsSessionRequest> for Op
where
    for<'a> &'a Op: Operation<PaymentsSessionRequest>,
{
    #[instrument(skip_all)]
    async fn get_or_create_customer_details<'a>(
        &'a self,
        db: &dyn StorageInterface,
        payment_data: &mut PaymentData,
        request: Option<payments::CustomerDetails>,
        merchant_id: &str,
    ) -> errors::CustomResult<
        (
            BoxedOperation<'a, PaymentsSessionRequest>,
            Option<storage::Customer>,
        ),
        errors::StorageError,
    > {
        helpers::create_customer_if_not_exist(
            Box::new(self),
            db,
            payment_data,
            request,
            merchant_id,
        )
        .await
    }

    #[instrument(skip_all)]
    async fn make_pm_data<'b>(
        &'b self,
        _state: &'b AppState,
        _payment_data: &mut PaymentData,
        _storage_scheme: enums::MerchantStorageScheme,
    ) -> RouterResult<(
        BoxedOperation<'b, PaymentsSessionRequest>,
        Option<api::PaymentMethod>,
    )> {
        //No payment method data for this operation
        Ok((Box::new(self), None))
    }

    async fn get_connector<'a>(
        &'a self,
        merchant_account: &storage::MerchantAccount,
        state: &AppState,
        request: &PaymentsSessionRequest,
    ) -> RouterResult<api::ConnectorCallType> {
        let connectors = &state.conf.connectors;
        let db = &state.store;

        let supported_connectors: &Vec<String> = state.conf.connectors.supported.wallets.as_ref();

        let connector_accounts = db
            .find_merchant_connector_account_by_merchant_id_list(&merchant_account.merchant_id)
            .await
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Database error when querying for merchant connector accounts")?;

        let normal_connector_names = connector_accounts
            .iter()
            .filter(|connector_account| {
                supported_connectors.contains(&connector_account.connector_name)
            })
            .map(|filtered_connector| filtered_connector.connector_name.clone())
            .collect::<HashSet<String>>();

        // Parse the payment methods enabled to check if the merchant has enabled gpay ( wallet )
        // through that connector. This parsing from serde_json::Value to payment method is costly and has to be done for every connector
        // for sure looks like an area of optimization
        let session_token_from_metadata_connectors = connector_accounts
            .iter()
            .filter(|connector_account| {
                connector_account
                    .payment_methods_enabled
                    .clone()
                    .unwrap_or_default()
                    .iter()
                    .any(|payment_method| {
                        let parsed_payment_method: PaymentMethodEnabled = payment_method
                            .clone()
                            .parse_value("payment_method")
                            .unwrap_or_default();

                        parsed_payment_method.payment_method == "wallet"
                    })
            })
            .map(|filtered_connector| filtered_connector.connector_name.clone())
            .collect::<HashSet<String>>();

        let given_wallets = request.wallets.clone();

        let connectors_data = if !given_wallets.is_empty() {
            // Create connectors for provided wallets
            let mut connectors_data = Vec::with_capacity(supported_connectors.len());
            for wallet in given_wallets {
                let (connector_name, connector_type) = match wallet {
                    api_enums::SupportedWallets::Gpay => ("adyen", api::GetToken::Metadata),
                    api_enums::SupportedWallets::ApplePay => ("applepay", api::GetToken::Connector),
                    api_enums::SupportedWallets::Paypal => ("braintree", api::GetToken::Connector),
                    api_enums::SupportedWallets::Klarna => ("klarna", api::GetToken::Connector),
                };

                // Check if merchant has enabled the required merchant connector account
                if session_token_from_metadata_connectors.contains(connector_name)
                    || normal_connector_names.contains(connector_name)
                {
                    connectors_data.push(api::ConnectorData::get_connector_by_name(
                        connectors,
                        connector_name,
                        connector_type,
                    )?);
                }
            }
            connectors_data
        } else {
            // Create connectors for all enabled wallets
            let mut connectors_data = Vec::with_capacity(
                normal_connector_names.len() + session_token_from_metadata_connectors.len(),
            );

            for connector_name in normal_connector_names {
                let connector_data = api::ConnectorData::get_connector_by_name(
                    connectors,
                    &connector_name,
                    api::GetToken::Connector,
                )?;
                connectors_data.push(connector_data);
            }

            for connector_name in session_token_from_metadata_connectors {
                let connector_data = api::ConnectorData::get_connector_by_name(
                    connectors,
                    &connector_name,
                    api::GetToken::Metadata,
                )?;
                connectors_data.push(connector_data);
            }
            connectors_data
        };

        Ok(api::ConnectorCallType::Multiple(connectors_data))
    }
}

impl<FData> DeriveFlow<api::Session, FData> for PaymentSession
where
    PaymentData: payments::flows::ConstructFlowSpecificData<
        api::Session,
        FData,
        crate::types::PaymentsResponseData,
    >,
    types::RouterData<api::Session, FData, crate::types::PaymentsResponseData>:
        payments::flows::Feature<api::Session, FData>,
    (dyn api::Connector + 'static):
        services::api::ConnectorIntegration<api::Session, FData, types::PaymentsResponseData>,
    operations::payment_response::PaymentResponse: operations::EndOperation<api::Session, FData>,
    FData: Send,
{
    fn should_call_connector(&self, _payment_data: &PaymentData) -> bool {
        true
    }
}
