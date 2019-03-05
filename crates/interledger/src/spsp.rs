use base64;
use bytes::Bytes;
use futures::Future;
use hyper::Server;
use interledger_btp::{connect_client, parse_btp_url};
use interledger_http::HttpClientService;
use interledger_ildcp::get_ildcp_info;
use interledger_router::Router;
use interledger_service_util::{RejecterService, ValidatorService};
use interledger_spsp::{pay, spsp_responder};
use interledger_store_memory::{Account, AccountBuilder, InMemoryStore};
use interledger_stream::StreamReceiverService;
use ring::rand::{SecureRandom, SystemRandom};
use std::net::SocketAddr;
use std::u64;
use tokio;
use url::Url;

const ACCOUNT_ID: u64 = 0;

pub fn random_token() -> String {
    let mut bytes: [u8; 18] = [0; 18];
    SystemRandom::new().fill(&mut bytes).unwrap();
    base64::encode_config(&bytes, base64::URL_SAFE_NO_PAD)
}

pub fn random_secret() -> [u8; 32] {
    let mut bytes: [u8; 32] = [0; 32];
    SystemRandom::new().fill(&mut bytes).unwrap();
    bytes
}

pub fn send_spsp_payment_btp(btp_server: &str, receiver: &str, amount: u64, quiet: bool) {
    let receiver = receiver.to_string();
    let account = AccountBuilder::new()
        .additional_routes(&[&b""[..]])
        .btp_uri(Url::parse(btp_server).unwrap())
        .build();
    let store = InMemoryStore::from_accounts(vec![account.clone()]);
    let run = connect_client(
        RejecterService::default(),
        RejecterService::default(),
        store.clone(),
        vec![ACCOUNT_ID],
    )
    .map_err(|err| {
        eprintln!("Error connecting to BTP server: {:?}", err);
        eprintln!("(Hint: is moneyd running?)");
    })
    .and_then(move |service| {
        let router = Router::new(service, store);
        pay(router, account, &receiver, amount)
            .map_err(|err| {
                eprintln!("Error sending SPSP payment: {:?}", err);
            })
            .and_then(move |delivered| {
                if !quiet {
                    println!(
                        "Sent: {}, delivered: {} (in the receiver's units)",
                        amount, delivered
                    );
                }
                Ok(())
            })
    });
    tokio::run(run);
}

pub fn send_spsp_payment_http(http_server: &str, receiver: &str, amount: u64, quiet: bool) {
    let receiver = receiver.to_string();
    let url = Url::parse(http_server).expect("Cannot parse HTTP URL");
    let auth_header = if !url.username().is_empty() {
        Some(format!(
            "Basic {}",
            base64::encode(&format!(
                "{}:{}",
                url.username(),
                url.password().unwrap_or("")
            ))
        ))
    } else if let Some(password) = url.password() {
        Some(format!("Bearer {}", password))
    } else {
        None
    };
    let account = if let Some(auth_header) = auth_header {
        AccountBuilder::new()
            .additional_routes(&[&b""[..]])
            .http_endpoint(Url::parse(http_server).unwrap())
            .http_outgoing_authorization(auth_header)
            .build()
    } else {
        AccountBuilder::new()
            .additional_routes(&[&b""[..]])
            .http_endpoint(Url::parse(http_server).unwrap())
            .build()
    };
    let store = InMemoryStore::from_accounts(vec![account.clone()]);
    let service = ValidatorService::outgoing(HttpClientService::new(store.clone()));
    let router = Router::new(service, store);
    let run = pay(router, account, &receiver, amount)
        .map_err(|err| {
            eprintln!("Error sending SPSP payment: {:?}", err);
        })
        .and_then(move |delivered| {
            if !quiet {
                println!(
                    "Sent: {}, delivered: {} (in the receiver's units)",
                    amount, delivered
                );
            }
            Ok(())
        });
    tokio::run(run);
}

// TODO allow server secret to be specified
pub fn run_spsp_server_btp(btp_server: &str, address: SocketAddr, _quiet: bool) {
    let account: Account = AccountBuilder::new()
        .additional_routes(&[&b""[..]])
        .btp_uri(parse_btp_url(btp_server).unwrap())
        .build();
    let secret = random_secret();
    let store = InMemoryStore::from_accounts(vec![account.clone()]);
    let stream_server = StreamReceiverService::without_ildcp(&secret, RejecterService::default());

    let run = connect_client(
        ValidatorService::incoming(stream_server.clone()),
        RejecterService::default(),
        store.clone(),
        vec![ACCOUNT_ID],
    )
    .map_err(|err| {
        eprintln!("Error connecting to BTP server: {:?}", err);
        eprintln!("(Hint: is moneyd running?)");
    })
    .and_then(move |btp_service| {
        let btp_service = ValidatorService::outgoing(btp_service);
        let mut router = Router::new(btp_service, store);
        get_ildcp_info(&mut router, account.clone()).and_then(move |info| {
            let client_address = Bytes::from(info.client_address());

            stream_server.set_ildcp(info);

            Server::bind(&address)
                .serve(move || spsp_responder(&client_address[..], &secret[..]))
                .map_err(|e| eprintln!("Server error: {:?}", e))
        })
    });
    tokio::run(run);
}
