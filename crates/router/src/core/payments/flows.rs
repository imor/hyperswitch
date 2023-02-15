pub mod authorize_flow;
pub mod cancel_flow;
pub mod capture_flow;
pub mod psync_flow;
pub mod session_flow;
pub mod verfiy_flow;

use async_trait::async_trait;

use crate::{
    connector,
    core::{errors::RouterResult, payments},
    routes::AppState,
    services,
    types::{self, api, storage},
};

#[async_trait]
pub trait ConstructFlowSpecificData<F, Req, Res> {
    async fn construct_router_data<'a>(
        &self,
        state: &AppState,
        connector_id: &str,
        merchant_account: &storage::MerchantAccount,
    ) -> RouterResult<types::RouterData<F, Req, Res>>;
}

#[async_trait]
pub trait Feature<F, T> {
    async fn decide_flows<'a>(
        self,
        state: &AppState,
        connector: &api::ConnectorData,
        maybe_customer: &Option<storage::Customer>,
        call_connector_action: payments::CallConnectorAction,
        merchant_account: &storage::MerchantAccount,
        session_token: Option<types::SessionTokenResult>,
    ) -> RouterResult<Self>
    where
        Self: Sized,
        F: Clone,
        dyn api::Connector: services::ConnectorIntegration<F, T, types::PaymentsResponseData>;

    async fn add_access_token<'a>(
        &self,
        state: &AppState,
        connector: &api::ConnectorData,
        merchant_account: &storage::MerchantAccount,
    ) -> RouterResult<types::AddAccessTokenResult>
    where
        F: Clone,
        Self: Sized,
        dyn api::Connector: services::ConnectorIntegration<F, T, types::PaymentsResponseData>;

    async fn get_session_token<'a>(
        &self,
        _state: &AppState,
        _connector: &api::ConnectorData,
    ) -> RouterResult<Option<types::SessionTokenResult>>
    where
        Self: Sized,
        F: Clone,
        dyn api::Connector: services::ConnectorIntegration<F, T, types::PaymentsResponseData>,
    {
        Ok(None)
    }
}

macro_rules! default_imp_for_sessions{
    ($($path:ident::$connector:ident),*)=> {
        $(impl
            services::ConnectorIntegration<
            api::PreAuthorize,
            types::PreAuthorizeData,
            types::PaymentsResponseData,
        > for $path::$connector
        {}
    )*
    };
}

default_imp_for_sessions!(
    connector::Aci,
    connector::Adyen,
    connector::Applepay,
    connector::Authorizedotnet,
    connector::Braintree,
    connector::Checkout,
    connector::Cybersource,
    connector::Fiserv,
    connector::Globalpay,
    connector::Klarna,
    connector::Payu,
    connector::Rapyd,
    connector::Shift4,
    connector::Stripe,
    connector::Worldline,
    connector::Worldpay
);
