use std::{fmt::Debug, marker::PhantomData};

use error_stack::{IntoReport, ResultExt};
use router_env::{instrument, tracing};

use super::{flows::Feature, PaymentAddress, PaymentData};
use crate::{
    configs::settings::Server,
    connector::Paypal,
    core::{
        errors::{self, RouterResponse, RouterResult},
        payments::{self, helpers},
    },
    routes::AppState,
    services::{self, RedirectForm},
    types::{
        self, api,
        storage::{self, enums},
        transformers::{ForeignFrom, ForeignInto},
    },
    utils::{OptionExt, ValueExt},
};

#[instrument(skip_all)]
pub async fn construct_payment_router_data<'a, F, T>(
    state: &'a AppState,
    payment_data: PaymentData<F>,
    connector_id: &str,
    merchant_account: &storage::MerchantAccount,
) -> RouterResult<types::RouterData<F, T, types::PaymentsResponseData>>
where
    T: TryFrom<PaymentAdditionalData<'a, F>>,
    types::RouterData<F, T, types::PaymentsResponseData>: Feature<F, T>,
    F: Clone,
    error_stack::Report<errors::ApiErrorResponse>:
        From<<T as TryFrom<PaymentAdditionalData<'a, F>>>::Error>,
{
    let (merchant_connector_account, payment_method, router_data);
    let connector_label = helpers::get_connector_label(
        payment_data.payment_intent.business_country,
        &payment_data.payment_intent.business_label,
        payment_data.payment_attempt.business_sub_label.as_ref(),
        connector_id,
    );

    let db = &*state.store;
    merchant_connector_account = helpers::get_merchant_connector_account(
        db,
        merchant_account.merchant_id.as_str(),
        &connector_label,
        payment_data.creds_identifier.to_owned(),
    )
    .await?;

    let auth_type: types::ConnectorAuthType = merchant_connector_account
        .get_connector_account_details()
        .parse_value("ConnectorAuthType")
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Failed while parsing value for ConnectorAuthType")?;

    payment_method = payment_data
        .payment_attempt
        .payment_method
        .or(payment_data.payment_attempt.payment_method)
        .get_required_value("payment_method_type")?;

    // [#44]: why should response be filled during request
    let response = payment_data
        .payment_attempt
        .connector_transaction_id
        .as_ref()
        .map(|id| types::PaymentsResponseData::TransactionResponse {
            resource_id: types::ResponseId::ConnectorTransactionId(id.to_string()),
            redirection_data: None,
            mandate_reference: None,
            connector_metadata: None,
        });

    let additional_data = PaymentAdditionalData {
        router_base_url: state.conf.server.base_url.clone(),
        connector_name: connector_id.to_string(),
        payment_data: payment_data.clone(),
        state,
    };

    router_data = types::RouterData {
        flow: PhantomData,
        merchant_id: merchant_account.merchant_id.clone(),
        connector: connector_id.to_owned(),
        payment_id: payment_data.payment_attempt.payment_id.clone(),
        attempt_id: payment_data.payment_attempt.attempt_id.clone(),
        status: payment_data.payment_attempt.status,
        payment_method,
        connector_auth_type: auth_type,
        description: payment_data.payment_intent.description.clone(),
        return_url: payment_data.payment_intent.return_url.clone(),
        payment_method_id: payment_data.payment_attempt.payment_method_id.clone(),
        address: payment_data.address.clone(),
        auth_type: payment_data
            .payment_attempt
            .authentication_type
            .unwrap_or_default(),
        connector_meta_data: merchant_connector_account.get_metadata(),
        request: T::try_from(additional_data)?,
        response: response.map_or_else(|| Err(types::ErrorResponse::default()), Ok),
        amount_captured: payment_data.payment_intent.amount_captured,
        access_token: None,
        session_token: None,
        reference_id: None,
        payment_method_token: payment_data.pm_token,
    };

    Ok(router_data)
}

pub trait ToResponse<Req, D, Op>
where
    Self: Sized,
    Op: Debug,
{
    fn generate_response(
        req: Option<Req>,
        data: D,
        customer: Option<storage::Customer>,
        auth_flow: services::AuthFlow,
        server: &Server,
        operation: Op,
    ) -> RouterResponse<Self>;
}

impl<F, Req, Op> ToResponse<Req, PaymentData<F>, Op> for api::PaymentsResponse
where
    F: Clone,
    Op: Debug,
{
    fn generate_response(
        req: Option<Req>,
        payment_data: PaymentData<F>,
        customer: Option<storage::Customer>,
        auth_flow: services::AuthFlow,
        server: &Server,
        operation: Op,
    ) -> RouterResponse<Self> {
        payments_to_payments_response(
            req,
            payment_data.payment_attempt,
            payment_data.payment_intent,
            payment_data.refunds,
            payment_data.payment_method_data,
            customer,
            auth_flow,
            payment_data.address,
            server,
            payment_data.connector_response.authentication_data,
            operation,
        )
    }
}

impl<F, Req, Op> ToResponse<Req, PaymentData<F>, Op> for api::PaymentsSessionResponse
where
    Self: From<Req>,
    F: Clone,
    Op: Debug,
{
    fn generate_response(
        _req: Option<Req>,
        payment_data: PaymentData<F>,
        _customer: Option<storage::Customer>,
        _auth_flow: services::AuthFlow,
        _server: &Server,
        _operation: Op,
    ) -> RouterResponse<Self> {
        Ok(services::ApplicationResponse::Json(Self {
            session_token: payment_data.sessions_token,
            payment_id: payment_data.payment_attempt.payment_id,
            client_secret: payment_data
                .payment_intent
                .client_secret
                .get_required_value("client_secret")?
                .into(),
        }))
    }
}

impl<F, Req, Op> ToResponse<Req, PaymentData<F>, Op> for api::VerifyResponse
where
    Self: From<Req>,
    F: Clone,
    Op: Debug,
{
    fn generate_response(
        _req: Option<Req>,
        data: PaymentData<F>,
        customer: Option<storage::Customer>,
        _auth_flow: services::AuthFlow,
        _server: &Server,
        _operation: Op,
    ) -> RouterResponse<Self> {
        Ok(services::ApplicationResponse::Json(Self {
            verify_id: Some(data.payment_intent.payment_id),
            merchant_id: Some(data.payment_intent.merchant_id),
            client_secret: data.payment_intent.client_secret.map(masking::Secret::new),
            customer_id: customer.as_ref().map(|x| x.customer_id.clone()),
            email: customer
                .as_ref()
                .and_then(|cus| cus.email.as_ref().map(|s| s.to_owned())),
            name: customer
                .as_ref()
                .and_then(|cus| cus.name.as_ref().map(|s| s.to_owned().into())),
            phone: customer
                .as_ref()
                .and_then(|cus| cus.phone.as_ref().map(|s| s.to_owned())),
            mandate_id: data.mandate_id.map(|mandate_ids| mandate_ids.mandate_id),
            payment_method: data
                .payment_attempt
                .payment_method
                .map(ForeignInto::foreign_into),
            payment_method_data: data
                .payment_method_data
                .map(api::PaymentMethodDataResponse::from),
            payment_token: data.token,
            error_code: data.payment_attempt.error_code,
            error_message: data.payment_attempt.error_message,
        }))
    }
}

#[instrument(skip_all)]
// try to use router data here so that already validated things , we don't want to repeat the validations.
// Add internal value not found and external value not found so that we can give 500 / Internal server error for internal value not found
#[allow(clippy::too_many_arguments)]
pub fn payments_to_payments_response<R, Op>(
    payment_request: Option<R>,
    payment_attempt: storage::PaymentAttempt,
    payment_intent: storage::PaymentIntent,
    refunds: Vec<storage::Refund>,
    payment_method_data: Option<api::PaymentMethodData>,
    customer: Option<storage::Customer>,
    auth_flow: services::AuthFlow,
    address: PaymentAddress,
    server: &Server,
    redirection_data: Option<serde_json::Value>,
    operation: Op,
) -> RouterResponse<api::PaymentsResponse>
where
    Op: Debug,
{
    let currency = payment_attempt
        .currency
        .as_ref()
        .get_required_value("currency")?
        .to_string();
    let mandate_id = payment_attempt.mandate_id.clone();
    let refunds_response = if refunds.is_empty() {
        None
    } else {
        Some(refunds.into_iter().map(ForeignInto::foreign_into).collect())
    };

    Ok(match payment_request {
        Some(_request) => {
            if payments::is_start_pay(&operation) && redirection_data.is_some() {
                let redirection_data = redirection_data.get_required_value("redirection_data")?;
                let form: RedirectForm = serde_json::from_value(redirection_data)
                    .map_err(|_| errors::ApiErrorResponse::InternalServerError)?;
                services::ApplicationResponse::Form(form)
            } else {
                let mut next_action_response = None;
                if payment_intent.status == enums::IntentStatus::RequiresCustomerAction {
                    next_action_response = Some(api::NextAction {
                        next_action_type: api::NextActionType::RedirectToUrl,
                        redirect_to_url: Some(helpers::create_startpay_url(
                            server,
                            &payment_attempt,
                            &payment_intent,
                        )),
                    })
                }
                let mut response: api::PaymentsResponse = Default::default();
                let routed_through = payment_attempt.connector.clone();

                let connector_label = routed_through.as_ref().map(|connector_name| {
                    helpers::get_connector_label(
                        payment_intent.business_country,
                        &payment_intent.business_label,
                        payment_attempt.business_sub_label.as_ref(),
                        connector_name,
                    )
                });
                let parsed_metadata: Option<api_models::payments::Metadata> = payment_intent
                    .metadata
                    .clone()
                    .map(|metadata_value| {
                        metadata_value
                            .parse_value("metadata")
                            .change_context(errors::ApiErrorResponse::InvalidDataValue {
                                field_name: "metadata",
                            })
                            .attach_printable("unable to parse metadata")
                    })
                    .transpose()
                    .unwrap_or_default();
                services::ApplicationResponse::Json(
                    response
                        .set_payment_id(Some(payment_attempt.payment_id))
                        .set_merchant_id(Some(payment_attempt.merchant_id))
                        .set_status(payment_intent.status.foreign_into())
                        .set_amount(payment_attempt.amount)
                        .set_amount_capturable(None)
                        .set_amount_received(payment_intent.amount_captured)
                        .set_connector(routed_through)
                        .set_client_secret(payment_intent.client_secret.map(masking::Secret::new))
                        .set_created(Some(payment_intent.created_at))
                        .set_currency(currency)
                        .set_customer_id(customer.as_ref().map(|cus| cus.clone().customer_id))
                        .set_email(
                            customer
                                .as_ref()
                                .and_then(|cus| cus.email.as_ref().map(|s| s.to_owned())),
                        )
                        .set_name(
                            customer
                                .as_ref()
                                .and_then(|cus| cus.name.as_ref().map(|s| s.to_owned().into())),
                        )
                        .set_phone(
                            customer
                                .as_ref()
                                .and_then(|cus| cus.phone.as_ref().map(|s| s.to_owned())),
                        )
                        .set_mandate_id(mandate_id)
                        .set_description(payment_intent.description)
                        .set_refunds(refunds_response) // refunds.iter().map(refund_to_refund_response),
                        .set_payment_method(
                            payment_attempt
                                .payment_method
                                .map(ForeignInto::foreign_into),
                            auth_flow == services::AuthFlow::Merchant,
                        )
                        .set_payment_method_data(
                            payment_method_data.map(api::PaymentMethodDataResponse::from),
                            auth_flow == services::AuthFlow::Merchant,
                        )
                        .set_payment_token(payment_attempt.payment_token)
                        .set_error_message(payment_attempt.error_message)
                        .set_error_code(payment_attempt.error_code)
                        .set_shipping(address.shipping)
                        .set_billing(address.billing)
                        .set_next_action(next_action_response)
                        .set_return_url(payment_intent.return_url)
                        .set_cancellation_reason(payment_attempt.cancellation_reason)
                        .set_authentication_type(
                            payment_attempt
                                .authentication_type
                                .map(ForeignInto::foreign_into),
                        )
                        .set_statement_descriptor_name(payment_intent.statement_descriptor_name)
                        .set_statement_descriptor_suffix(payment_intent.statement_descriptor_suffix)
                        .set_setup_future_usage(
                            payment_intent
                                .setup_future_usage
                                .map(ForeignInto::foreign_into),
                        )
                        .set_capture_method(
                            payment_attempt
                                .capture_method
                                .map(ForeignInto::foreign_into),
                        )
                        .set_payment_experience(
                            payment_attempt
                                .payment_experience
                                .map(ForeignInto::foreign_into),
                        )
                        .set_payment_method_type(
                            payment_attempt
                                .payment_method_type
                                .map(ForeignInto::foreign_into),
                        )
                        .set_metadata(payment_intent.metadata)
                        .set_connector_label(connector_label)
                        .set_business_country(payment_intent.business_country)
                        .set_business_label(payment_intent.business_label)
                        .set_business_sub_label(payment_attempt.business_sub_label)
                        .set_allowed_payment_method_types(
                            parsed_metadata
                                .and_then(|metadata| metadata.allowed_payment_method_types),
                        )
                        .to_owned(),
                )
            }
        }
        None => services::ApplicationResponse::Json(api::PaymentsResponse {
            payment_id: Some(payment_attempt.payment_id),
            merchant_id: Some(payment_attempt.merchant_id),
            status: payment_intent.status.foreign_into(),
            amount: payment_attempt.amount,
            amount_capturable: None,
            amount_received: payment_intent.amount_captured,
            client_secret: payment_intent.client_secret.map(masking::Secret::new),
            created: Some(payment_intent.created_at),
            currency,
            customer_id: payment_intent.customer_id,
            description: payment_intent.description,
            refunds: refunds_response,
            payment_method: payment_attempt
                .payment_method
                .map(ForeignInto::foreign_into),
            capture_method: payment_attempt
                .capture_method
                .map(ForeignInto::foreign_into),
            error_message: payment_attempt.error_message,
            error_code: payment_attempt.error_code,
            payment_method_data: payment_method_data.map(api::PaymentMethodDataResponse::from),
            email: customer
                .as_ref()
                .and_then(|cus| cus.email.as_ref().map(|s| s.to_owned())),
            name: customer
                .as_ref()
                .and_then(|cus| cus.name.as_ref().map(|s| s.to_owned().into())),
            phone: customer
                .as_ref()
                .and_then(|cus| cus.phone.as_ref().map(|s| s.to_owned())),
            mandate_id,
            shipping: address.shipping,
            billing: address.billing,
            cancellation_reason: payment_attempt.cancellation_reason,
            payment_token: payment_attempt.payment_token,
            metadata: payment_intent.metadata,
            ..Default::default()
        }),
    })
}

impl ForeignFrom<(storage::PaymentIntent, storage::PaymentAttempt)> for api::PaymentsResponse {
    fn foreign_from(item: (storage::PaymentIntent, storage::PaymentAttempt)) -> Self {
        let pi = item.0;
        let pa = item.1;
        Self {
            payment_id: Some(pi.payment_id),
            merchant_id: Some(pi.merchant_id),
            status: pi.status.foreign_into(),
            amount: pi.amount,
            amount_capturable: pi.amount_captured,
            client_secret: pi.client_secret.map(|s| s.into()),
            created: Some(pi.created_at),
            currency: pi.currency.map(|c| c.to_string()).unwrap_or_default(),
            description: pi.description,
            metadata: pi.metadata,
            customer_id: pi.customer_id,
            connector: pa.connector,
            payment_method: pa.payment_method.map(ForeignInto::foreign_into),
            payment_method_type: pa.payment_method_type.map(ForeignInto::foreign_into),
            ..Default::default()
        }
    }
}

#[derive(Clone)]
pub struct PaymentAdditionalData<'a, F>
where
    F: Clone,
{
    router_base_url: String,
    connector_name: String,
    payment_data: PaymentData<F>,
    state: &'a AppState,
}
impl<F: Clone> TryFrom<PaymentAdditionalData<'_, F>> for types::PaymentsAuthorizeData {
    type Error = error_stack::Report<errors::ApiErrorResponse>;

    fn try_from(additional_data: PaymentAdditionalData<'_, F>) -> Result<Self, Self::Error> {
        let payment_data = additional_data.payment_data;
        let router_base_url = &additional_data.router_base_url;
        let connector_name = &additional_data.connector_name;
        let attempt = &payment_data.payment_attempt;
        let browser_info: Option<types::BrowserInformation> = attempt
            .browser_info
            .clone()
            .map(|b| b.parse_value("BrowserInformation"))
            .transpose()
            .change_context(errors::ApiErrorResponse::InvalidDataValue {
                field_name: "browser_info",
            })?;

        let parsed_metadata: Option<api_models::payments::Metadata> = payment_data
            .payment_intent
            .metadata
            .map(|metadata_value| {
                metadata_value
                    .parse_value("metadata")
                    .change_context(errors::ApiErrorResponse::InvalidDataValue {
                        field_name: "metadata",
                    })
                    .attach_printable("unable to parse metadata")
            })
            .transpose()
            .unwrap_or_default();

        let order_details = parsed_metadata.and_then(|data| data.order_details);
        let complete_authorize_url = Some(helpers::create_complete_authorize_url(
            router_base_url,
            attempt,
            connector_name,
        ));
        let webhook_url = Some(helpers::create_webhook_url(
            router_base_url,
            attempt,
            connector_name,
        ));
        let router_return_url = Some(helpers::create_redirect_url(
            router_base_url,
            attempt,
            connector_name,
            payment_data.creds_identifier.as_deref(),
        ));

        Ok(Self {
            payment_method_data: payment_data
                .payment_method_data
                .get_required_value("payment_method_data")?,
            setup_future_usage: payment_data.payment_intent.setup_future_usage,
            mandate_id: payment_data.mandate_id.clone(),
            off_session: payment_data.mandate_id.as_ref().map(|_| true),
            setup_mandate_details: payment_data.setup_mandate.clone(),
            confirm: payment_data.payment_attempt.confirm,
            statement_descriptor_suffix: payment_data.payment_intent.statement_descriptor_suffix,
            statement_descriptor: payment_data.payment_intent.statement_descriptor_name,
            capture_method: payment_data.payment_attempt.capture_method,
            amount: payment_data.amount.into(),
            currency: payment_data.currency,
            browser_info,
            email: payment_data.email,
            payment_experience: payment_data.payment_attempt.payment_experience,
            order_details,
            session_token: None,
            enrolled_for_3ds: true,
            related_transaction_id: None,
            payment_method_type: payment_data.payment_attempt.payment_method_type,
            router_return_url,
            webhook_url,
            complete_authorize_url,
        })
    }
}

impl<F: Clone> TryFrom<PaymentAdditionalData<'_, F>> for types::PaymentsSyncData {
    type Error = errors::ApiErrorResponse;

    fn try_from(additional_data: PaymentAdditionalData<'_, F>) -> Result<Self, Self::Error> {
        let payment_data = additional_data.payment_data;
        Ok(Self {
            connector_transaction_id: match payment_data.payment_attempt.connector_transaction_id {
                Some(connector_txn_id) => {
                    types::ResponseId::ConnectorTransactionId(connector_txn_id)
                }
                None => types::ResponseId::NoResponseId,
            },
            encoded_data: payment_data.connector_response.encoded_data,
            capture_method: payment_data.payment_attempt.capture_method,
            connector_meta: payment_data.payment_attempt.connector_metadata,
        })
    }
}

impl api::ConnectorTransactionId for Paypal {
    fn connector_transaction_id(
        &self,
        payment_attempt: storage::PaymentAttempt,
    ) -> Result<Option<String>, errors::ApiErrorResponse> {
        let payment_method = payment_attempt.payment_method;
        let metadata = Self::connector_transaction_id(
            self,
            payment_method,
            &payment_attempt.connector_metadata,
        );
        match metadata {
            Ok(data) => Ok(data),
            _ => Err(errors::ApiErrorResponse::ResourceIdNotFound),
        }
    }
}

impl<F: Clone> TryFrom<PaymentAdditionalData<'_, F>> for types::PaymentsCaptureData {
    type Error = error_stack::Report<errors::ApiErrorResponse>;

    fn try_from(additional_data: PaymentAdditionalData<'_, F>) -> Result<Self, Self::Error> {
        let payment_data = additional_data.payment_data;
        let connector = api::ConnectorData::get_connector_by_name(
            &additional_data.state.conf.connectors,
            &additional_data.connector_name,
            api::GetToken::Connector,
        )?;
        let amount_to_capture: i64 = payment_data
            .payment_attempt
            .amount_to_capture
            .map_or(payment_data.amount.into(), |capture_amount| capture_amount);
        Ok(Self {
            amount_to_capture,
            currency: payment_data.currency,
            connector_transaction_id: connector
                .connector
                .connector_transaction_id(payment_data.payment_attempt.clone())?
                .ok_or(errors::ApiErrorResponse::ResourceIdNotFound)?,
            payment_amount: payment_data.amount.into(),
            connector_meta: payment_data.payment_attempt.connector_metadata,
        })
    }
}

impl<F: Clone> TryFrom<PaymentAdditionalData<'_, F>> for types::PaymentsCancelData {
    type Error = error_stack::Report<errors::ApiErrorResponse>;

    fn try_from(additional_data: PaymentAdditionalData<'_, F>) -> Result<Self, Self::Error> {
        let payment_data = additional_data.payment_data;
        let connector = api::ConnectorData::get_connector_by_name(
            &additional_data.state.conf.connectors,
            &additional_data.connector_name,
            api::GetToken::Connector,
        )?;
        Ok(Self {
            amount: Some(payment_data.amount.into()),
            currency: Some(payment_data.currency),
            connector_transaction_id: connector
                .connector
                .connector_transaction_id(payment_data.payment_attempt.clone())?
                .ok_or(errors::ApiErrorResponse::ResourceIdNotFound)?,
            cancellation_reason: payment_data.payment_attempt.cancellation_reason,
            connector_meta: payment_data.payment_attempt.connector_metadata,
        })
    }
}

impl<F: Clone> TryFrom<PaymentAdditionalData<'_, F>> for types::PaymentsSessionData {
    type Error = error_stack::Report<errors::ApiErrorResponse>;

    fn try_from(additional_data: PaymentAdditionalData<'_, F>) -> Result<Self, Self::Error> {
        let payment_data = additional_data.payment_data;
        let parsed_metadata: Option<api_models::payments::Metadata> = payment_data
            .payment_intent
            .metadata
            .map(|metadata_value| {
                metadata_value
                    .parse_value("metadata")
                    .change_context(errors::ApiErrorResponse::InvalidDataValue {
                        field_name: "metadata",
                    })
                    .attach_printable("unable to parse metadata")
            })
            .transpose()
            .unwrap_or_default();

        let order_details = parsed_metadata.and_then(|data| data.order_details);

        Ok(Self {
            amount: payment_data.amount.into(),
            currency: payment_data.currency,
            country: payment_data.address.billing.and_then(|billing_address| {
                billing_address.address.and_then(|address| address.country)
            }),
            order_details,
        })
    }
}

impl<F: Clone> TryFrom<PaymentAdditionalData<'_, F>> for types::VerifyRequestData {
    type Error = error_stack::Report<errors::ApiErrorResponse>;

    fn try_from(additional_data: PaymentAdditionalData<'_, F>) -> Result<Self, Self::Error> {
        let payment_data = additional_data.payment_data;
        Ok(Self {
            currency: payment_data.currency,
            confirm: true,
            payment_method_data: payment_data
                .payment_method_data
                .get_required_value("payment_method_data")?,
            statement_descriptor_suffix: payment_data.payment_intent.statement_descriptor_suffix,
            setup_future_usage: payment_data.payment_intent.setup_future_usage,
            off_session: payment_data.mandate_id.as_ref().map(|_| true),
            mandate_id: payment_data.mandate_id.clone(),
            setup_mandate_details: payment_data.setup_mandate,
        })
    }
}

impl<F: Clone> TryFrom<PaymentAdditionalData<'_, F>> for types::CompleteAuthorizeData {
    type Error = error_stack::Report<errors::ApiErrorResponse>;

    fn try_from(additional_data: PaymentAdditionalData<'_, F>) -> Result<Self, Self::Error> {
        let payment_data = additional_data.payment_data;
        let browser_info: Option<types::BrowserInformation> = payment_data
            .payment_attempt
            .browser_info
            .clone()
            .map(|b| b.parse_value("BrowserInformation"))
            .transpose()
            .change_context(errors::ApiErrorResponse::InvalidDataValue {
                field_name: "browser_info",
            })?;

        let json_payload = payment_data
            .connector_response
            .encoded_data
            .map(|s| serde_json::from_str::<serde_json::Value>(&s))
            .transpose()
            .into_report()
            .change_context(errors::ApiErrorResponse::InternalServerError)?;
        Ok(Self {
            setup_future_usage: payment_data.payment_intent.setup_future_usage,
            mandate_id: payment_data.mandate_id.clone(),
            off_session: payment_data.mandate_id.as_ref().map(|_| true),
            setup_mandate_details: payment_data.setup_mandate.clone(),
            confirm: payment_data.payment_attempt.confirm,
            statement_descriptor_suffix: payment_data.payment_intent.statement_descriptor_suffix,
            capture_method: payment_data.payment_attempt.capture_method,
            amount: payment_data.amount.into(),
            currency: payment_data.currency,
            browser_info,
            email: payment_data.email,
            payment_method_data: payment_data.payment_method_data,
            connector_transaction_id: payment_data.connector_response.connector_transaction_id,
            payload: json_payload,
            connector_meta: payment_data.payment_attempt.connector_metadata,
        })
    }
}
