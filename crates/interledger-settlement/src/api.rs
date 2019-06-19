use crate::{SettlementAccount, SettlementStore};
use futures::{
    future::result,
    Future,
};
use hyper::Response;
use interledger_ildcp::IldcpAccount;
use interledger_packet::PrepareBuilder;
use interledger_service::{AccountStore, OutgoingRequest, OutgoingService};
use serde_json::Value;
use std::{
    marker::PhantomData,
    str::{self, FromStr},
    time::{Duration, SystemTime},
};

static PEER_PROTOCOL_CONDITION: [u8; 32] = [
    102, 104, 122, 173, 248, 98, 189, 119, 108, 143, 193, 139, 142, 159, 142, 32, 8, 151, 20, 133,
    110, 226, 51, 179, 144, 42, 89, 29, 13, 95, 41, 37,
];

pub struct SettlementApi<S, O, A> {
    outgoing_handler: O,
    store: S,
    account_type: PhantomData<A>,
}

#[derive(Extract, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SettlementDetails {
    pub amount: u64,
    pub scale: u32,
}

#[derive(Debug, Response)]
#[web(status = "200")]
struct Success;

// TODO add authentication

impl_web! {
    impl<S, O, A> SettlementApi<S, O, A>
    where
        S: SettlementStore<Account = A> + AccountStore<Account = A> + Clone + Send + Sync + 'static,
        O: OutgoingService<A> + Clone + Send + Sync + 'static,
        A: SettlementAccount + IldcpAccount + Send + Sync + 'static,
    {
        pub fn new(store: S, outgoing_handler: O) -> Self {
            SettlementApi {
                store,
                outgoing_handler,
                account_type: PhantomData,
            }
        }


        // TODO: The SE should retry until this is ACK’d so it needs to be idempotent,
        // https://stripe.com/docs/api/idempotent_requests?lang=curl
        // TODO: Can we make account_id: A::AccountId somehow?
        // derive(Extract) is not possible since it's inside a trait.
        // TODO: Can the Response<()> be converted to a Response<String>? It'd
        // be nice if we could include the full error message body (currently
        // it's just the header)
        #[post("/accounts/:account_id/settlement")]
        fn receive_settlement(&self, account_id: String, body: SettlementDetails) -> impl Future<Item = Success, Error = Response<()>> {
            let amount = body.amount;
            let _scale = body.scale; // todo: figure out how to use this, is it really necessary? should we check if it matches the SE details?
            let store = self.store.clone();
            let store_clone = store.clone();
            result(A::AccountId::from_str(&account_id)
                .map_err(move |_err| {
                    error!("Unable to parse account id: {}", account_id);
                    Response::builder().status(404).body(()).unwrap()
                }))
                .and_then(move |account_id| store.get_accounts(vec![account_id]).map_err(move |_| {
                    error!("Error getting account: {}", account_id);
                    Response::builder().status(404).body(()).unwrap()
                }))
                .and_then(|accounts| {
                    let account = &accounts[0];
                    if let Some(settlement_engine) = account.settlement_engine_details() {
                        Ok((account.clone(), settlement_engine))
                    } else {
                        error!("Account {} does not have settlement engine details configured. Cannot handle incoming settlement", account.id());
                        Err(Response::builder().status(404).body(()).unwrap())
                    }
                })
                .and_then(move |(account, settlement_engine)| {
                    let account_id = account.id(); // Get the account_id back

                    // TODO: Extract into a method since this is used in
                    // client.rs as well as the exchange_rates.rs service
                    let amount = if account.asset_scale() >= settlement_engine.asset_scale {
                        amount
                            * 10u64.pow(u32::from(
                                account.asset_scale() - settlement_engine.asset_scale,
                            ))
                    } else {
                        amount
                            / 10u64.pow(u32::from(
                                settlement_engine.asset_scale - account.asset_scale(),
                            ))
                    };

                    // TODO Idempotency header!
                    store_clone.update_balance_for_incoming_settlement(account_id, amount)
                        .map_err(move |_| {
                            error!("Error updating balance of account: {} for incoming settlement of amount: {}", account_id, amount);
                            Response::builder().status(201).body(()).unwrap() // Request was sent, but SE operation have failed.
                        })
                })
                .and_then(|_| Ok(Success))
        }

        // Gets called by our settlement engine, forwards the request outwards
        // until it reaches the peer's settlement engine
        #[post("/accounts/:account_id/messages")]
        fn send_outgoing_message(&self, account_id: String, body: String)-> impl Future<Item = Value, Error = Response<()>> {
            let store = self.store.clone();
            let mut outgoing_handler = self.outgoing_handler.clone();
            result(A::AccountId::from_str(&account_id)
                .map_err(move |_err| {
                    error!("Unable to parse account id: {}", account_id);
                    Response::builder().status(404).body(()).unwrap()
                }))
                .and_then(move |account_id| store.get_accounts(vec![account_id]).map_err(move |_| {
                    error!("Error getting account: {}", account_id);
                    Response::builder().status(404).body(()).unwrap()
                }))
                .and_then(|accounts| {
                    let account = &accounts[0];
                    if let Some(settlement_engine) = account.settlement_engine_details() {
                        Ok((account.clone(), settlement_engine))
                    } else {
                        error!("Account {} has no settlement engine details configured, cannot send a settlement engine message to that account", accounts[0].id());
                        Err(Response::builder().status(404).body(()).unwrap())
                    }
                })
                .and_then(move |(account, settlement_engine)| {
                    // Send the message to the peer's settlement engine.
                    // Note that we use dummy values for the `from` and `original_amount`
                    // because this `OutgoingRequest` will bypass the router and thus will not
                    // use either of these values. Including dummy values in the rare case where
                    // we do not need them seems easier than using
                    // `Option`s all over the place.
                    outgoing_handler.send_request(OutgoingRequest {
                        from: account.clone(),
                        to: account.clone(),
                        original_amount: 0,
                        prepare: PrepareBuilder {
                            destination: settlement_engine.ilp_address,
                            amount: 0,
                            expires_at: SystemTime::now() + Duration::from_secs(30),
                            data: body.as_ref(),
                            execution_condition: &PEER_PROTOCOL_CONDITION,
                        }.build()
                    })
                    .map_err(|reject| {
                        error!("Error sending message to peer settlement engine. Packet rejected with code: {}, message: {}", reject.code(), str::from_utf8(reject.message()).unwrap_or_default());
                        // spec: "Could not process sending of the message -> 400"
                        Response::builder().status(400).body(()).unwrap()
                    })
                })
                .and_then(|fulfill| {
                    serde_json::from_slice(fulfill.data()).map_err(|err| {
                        error!("Error parsing response from peer settlement engine as JSON: {:?}", err);
                        Response::builder().status(502).body(()).unwrap()
                    })
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;
    use crate::test_helpers::*;
    use std::sync::Arc;

    // Settlement Tests

    #[test]
    fn settlement_ok() {
        let id = TEST_ACCOUNT_0.clone().id.to_string();
        let store = test_store(false, true);
        let api = test_api(store);

        let ret = api.receive_settlement(id, SETTLEMENT_BODY.clone()).wait();
        assert!(ret.is_ok());
    }

    #[test]
    fn account_has_no_engine_configured() {
        let id = TEST_ACCOUNT_0.clone().id.to_string();
        let store = test_store(false, false);
        let api = test_api(store);

        let ret = api
            .receive_settlement(id, SETTLEMENT_BODY.clone())
            .wait()
            .unwrap_err();
        assert_eq!(ret.status().as_u16(), 404);
    }

    #[test]
    fn engine_rejects() {
        let id = TEST_ACCOUNT_0.clone().id.to_string();
        let store = test_store(true, true);
        let api = test_api(store);

        let ret: Response<_> = api
            .receive_settlement(id, SETTLEMENT_BODY.clone())
            .wait()
            .unwrap_err();
        assert_eq!(ret.status().as_u16(), 201);
    }

    #[test]
    fn invalid_account_id() {
        let id = "-1".to_string();
        let store = test_store(false, true);
        let api = test_api(store);

        let ret: Response<_> = api
            .receive_settlement(id, SETTLEMENT_BODY.clone())
            .wait()
            .unwrap_err();
        assert_eq!(ret.status().as_u16(), 404);
    }

    #[test]
    fn account_not_in_store() {
        let id = TEST_ACCOUNT_0.clone().id.to_string();
        let store = TestStore {
            accounts: Arc::new(vec![]),
            should_fail: false,
        };
        let api = test_api(store);

        let ret: Response<_> = api
            .receive_settlement(id, SETTLEMENT_BODY.clone())
            .wait()
            .unwrap_err();
        assert_eq!(ret.status().as_u16(), 404);
    }

    // Message Tests
}
