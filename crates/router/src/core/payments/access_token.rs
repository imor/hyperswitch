use std::fmt::Debug;

use common_utils::ext_traits::AsyncExt;
use error_stack::{IntoReport, ResultExt};

use super::helpers;
use crate::{
    core::{
        errors::{self, RouterResult},
        payments,
    },
    routes::AppState,
    services,
    types::{self, api as api_types, storage},
};

pub async fn add_access_token<
    F: Clone + 'static,
    Req: Debug + Clone + 'static,
    Res: Debug + Clone + 'static,
>(
    state: &AppState,
    connector: &api_types::ConnectorData,
    merchant_account: &storage::MerchantAccount,
    router_data: &types::RouterData<F, Req, Res>,
) -> RouterResult<types::AddAccessTokenResult> {
    if connector.connector_name.supports_access_token() {
        let merchant_id = &merchant_account.merchant_id;
        let store = &*state.store;
        let old_access_token = store
            .get_access_token(merchant_id, connector.connector.id())
            .await
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("DB error when accessing the access token")?;

        let res = match old_access_token {
            Some(access_token) => Ok(Some(access_token)),
            None => {
                let cloned_router_data = router_data.clone();
                let refresh_token_request_data = types::AccessTokenRequestData::try_from(
                    router_data.connector_auth_type.clone(),
                )
                .into_report()
                .attach_printable(
                    "Could not create access token request, invalid connector account credentials",
                )?;

                let refresh_token_response_data: Result<types::AccessToken, types::ErrorResponse> =
                    Err(types::ErrorResponse::default());
                let refresh_token_router_data = helpers::router_data_type_conversion::<
                    _,
                    api_types::AccessTokenAuth,
                    _,
                    _,
                    _,
                    _,
                >(
                    cloned_router_data,
                    refresh_token_request_data,
                    refresh_token_response_data,
                );
                refresh_connector_auth(
                    state,
                    connector,
                    merchant_account,
                    &refresh_token_router_data,
                )
                .await?
                .async_map(|access_token| async {
                    //Store the access token in db
                    let store = &*state.store;
                    // This error should not be propagated, we don't want payments to fail once we have
                    // the access token, the next request will create new access token
                    let _ = store
                        .set_access_token(
                            merchant_id,
                            connector.connector.id(),
                            access_token.clone(),
                        )
                        .await
                        .change_context(errors::ApiErrorResponse::InternalServerError)
                        .attach_printable("DB error when setting the access token");
                    Some(access_token)
                })
                .await
            }
        };

        Ok(types::AddAccessTokenResult {
            access_token_result: res,
            connector_supports_access_token: true,
        })
    } else {
        Ok(types::AddAccessTokenResult {
            access_token_result: Err(types::ErrorResponse::default()),
            connector_supports_access_token: false,
        })
    }
}

pub async fn refresh_connector_auth(
    state: &AppState,
    connector: &api_types::ConnectorData,
    _merchant_account: &storage::MerchantAccount,
    router_data: &types::RouterData<
        api_types::AccessTokenAuth,
        types::AccessTokenRequestData,
        types::AccessToken,
    >,
) -> RouterResult<Result<types::AccessToken, types::ErrorResponse>> {
    let connector_integration: services::BoxedConnectorIntegration<
        '_,
        api_types::AccessTokenAuth,
        types::AccessTokenRequestData,
        types::AccessToken,
    > = connector.connector.get_connector_integration();

    let access_token_router_data = services::execute_connector_processing_step(
        state,
        connector_integration,
        router_data,
        payments::CallConnectorAction::Trigger,
    )
    .await
    .change_context(errors::ApiErrorResponse::InternalServerError)
    .attach_printable("Could not refresh access token")?;

    Ok(access_token_router_data.response)
}
