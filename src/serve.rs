use std::collections::HashMap;
use std::{convert::Infallible, fmt::Debug, io, net::SocketAddr, path::PathBuf, rc::Rc, sync::Arc};

use clap::Parser;
use hex::FromHexError;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use soroban_env_host::{
    budget::Budget,
    storage::{AccessType, Footprint, Storage},
    xdr::{
        self, Error as XdrError, FeeBumpTransactionInnerTx, HostFunction, LedgerEntryData,
        LedgerKey, LedgerKeyContractData, OperationBody, ReadXdr, ScHostStorageErrorCode, ScObject,
        ScStatus, ScVal, TransactionEnvelope, WriteXdr,
    },
    Host, HostError,
};
use tokio::sync::Mutex;
use warp::{http::Response, Filter};

use crate::jsonrpc;
use crate::network::SANDBOX_NETWORK_PASSPHRASE;
use crate::snapshot;
use crate::strval::StrValError;
use crate::utils;

#[derive(Parser, Debug)]
pub struct Cmd {
    /// Port to listen for requests on.
    #[clap(long, default_value("8080"))]
    port: u16,
    /// File to persist ledger state
    #[clap(long, parse(from_os_str), default_value(".soroban/ledger.json"))]
    ledger_file: PathBuf,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("io")]
    Io(#[from] io::Error),
    #[error("strval")]
    StrVal(#[from] StrValError),
    #[error("xdr")]
    Xdr(#[from] XdrError),
    #[error("host")]
    Host(#[from] HostError),
    #[error("snapshot")]
    Snapshot(#[from] snapshot::Error),
    #[error("serde")]
    Serde(#[from] serde_json::Error),
    #[error("hex")]
    FromHex(#[from] FromHexError),
    #[error("unknownmethod")]
    UnknownMethod,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
#[serde(untagged)]
enum Requests {
    GetContractData((String, String)),
    StringArg(Box<[String]>),
}

impl Cmd {
    pub async fn run(&self) -> Result<(), Error> {
        let ledger_file = Arc::new(self.ledger_file.clone());
        let with_ledger_file = warp::any().map(move || ledger_file.clone());

        // Just track in-flight transactions in-memory for sandbox for now. Simple.
        let transaction_status_map: Arc<Mutex<HashMap<String, Value>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let with_transaction_status_map = warp::any().map(move || transaction_status_map.clone());

        let jsonrpc_route = warp::post()
            .and(warp::path!("api" / "v1" / "jsonrpc"))
            .and(warp::body::json())
            .and(with_ledger_file)
            .and(with_transaction_status_map)
            .and_then(handler);

        // Allow access from all remote sites when we are in local sandbox mode. (Always for now)
        let cors = warp::cors()
            .allow_any_origin()
            .allow_credentials(true)
            .allow_headers(vec![
                "Accept",
                "Access-Control-Request-Headers",
                "Access-Control-Request-Method",
                "Content-Length",
                "Content-Type",
                "Encoding",
                "Origin",
                "Referer",
                "Sec-Fetch-Mode",
                "User-Agent",
            ])
            .allow_methods(vec!["GET", "POST"]);
        let routes = jsonrpc_route.with(cors);

        let addr: SocketAddr = ([127, 0, 0, 1], self.port).into();
        println!("Listening on: {}", addr);
        warp::serve(routes).run(addr).await;
        Ok(())
    }
}

async fn handler(
    request: jsonrpc::Request<Requests>,
    ledger_file: Arc<PathBuf>,
    transaction_status_map: Arc<Mutex<HashMap<String, Value>>>,
) -> Result<impl warp::Reply, Infallible> {
    let resp = Response::builder()
        .status(200)
        .header("content-type", "application/json; charset=utf-8");
    if request.jsonrpc != "2.0" {
        return Ok(resp.body(
            json!({
                "jsonrpc": "2.0",
                "id": &request.id,
                "error": {
                    "code":-32600,
                    "message": "Invalid jsonrpc value in request",
                },
            })
            .to_string(),
        ));
    }
    let result = match (request.method.as_str(), request.params) {
        ("getContractData", Some(Requests::GetContractData((contract_id, key)))) => {
            get_contract_data(&contract_id, key, &ledger_file)
        }
        ("getTransactionStatus", Some(Requests::StringArg(b))) => {
            if let Some(hash) = b.into_vec().first() {
                let m = transaction_status_map.lock().await;
                let status = m.get(hash);
                Ok(match status {
                    Some(status) => status.clone(),
                    None => json!({
                        "error": {
                            "code":404,
                            "message": "Transaction not found",
                        },
                    }),
                })
            } else {
                Err(Error::Xdr(XdrError::Invalid))
            }
        }
        ("simulateTransaction", Some(Requests::StringArg(b))) => {
            if let Some(txn_xdr) = b.into_vec().first() {
                parse_transaction(txn_xdr, SANDBOX_NETWORK_PASSPHRASE)
                    // Execute and do NOT commit
                    .and_then(|(_, args)| execute_transaction(&args, &ledger_file, false))
            } else {
                Err(Error::Xdr(XdrError::Invalid))
            }
        }
        ("sendTransaction", Some(Requests::StringArg(b))) => {
            if let Some(txn_xdr) = b.into_vec().first() {
                // TODO: Format error object output if txn is invalid
                let mut m = transaction_status_map.lock().await;
                parse_transaction(txn_xdr, SANDBOX_NETWORK_PASSPHRASE).map(|(hash, args)| {
                    let id = hex::encode(hash);
                    // Execute and commit
                    let result = execute_transaction(&args, &ledger_file, true);
                    // Add it to our status tracker
                    m.insert(
                        id.clone(),
                        match result {
                            Ok(result) => {
                                json!({
                                    "id": id,
                                    "status": "success",
                                    "results": vec![result],
                                })
                            }
                            Err(_err) => {
                                // TODO: Actually render the real error to the user
                                // Add it to our status tracker
                                json!({
                                    "id": id,
                                    "status": "error",
                                    "error": {
                                        "code":-32603,
                                        "message": "Internal server error",
                                    },
                                })
                            }
                        },
                    );
                    // Return the hash
                    json!({ "id": id, "status": "pending" })
                })
            } else {
                Err(Error::Xdr(XdrError::Invalid))
            }
        }
        _ => Err(Error::UnknownMethod),
    };
    let r = reply(&request.id, result);
    Ok(resp.body(serde_json::to_string(&r).unwrap_or_else(|_| {
        json!({
            "jsonrpc": "2.0",
            "id": &request.id,
            "error": {
                "code":-32603,
                "message": "Internal server error",
            },
        })
        .to_string()
    })))
}

fn reply(
    id: &Option<jsonrpc::Id>,
    result: Result<Value, Error>,
) -> jsonrpc::Response<Value, Value> {
    match result {
        Ok(res) => jsonrpc::Response::Ok(jsonrpc::ResultResponse {
            jsonrpc: "2.0".to_string(),
            id: id.as_ref().unwrap_or(&jsonrpc::Id::Null).clone(),
            result: res,
        }),
        Err(err) => {
            eprintln!("err: {:?}", err);
            jsonrpc::Response::Err(jsonrpc::ErrorResponse {
                jsonrpc: "2.0".to_string(),
                id: id.as_ref().unwrap_or(&jsonrpc::Id::Null).clone(),
                error: jsonrpc::ErrorResponseError {
                    code: match err {
                        Error::Serde(_) => -32700,
                        Error::UnknownMethod => -32601,
                        _ => -32603,
                    },
                    message: err.to_string(),
                    data: None,
                },
            })
        }
    }
}

fn get_contract_data(
    contract_id_hex: &str,
    key_xdr: String,
    ledger_file: &PathBuf,
) -> Result<Value, Error> {
    // Initialize storage and host
    let ledger_entries = snapshot::read(ledger_file)?;
    let contract_id: [u8; 32] = utils::contract_id_from_str(&contract_id_hex.to_string())?;
    let key = ScVal::from_xdr_base64(key_xdr)?;

    let snap = Rc::new(snapshot::Snap { ledger_entries });
    let mut storage = Storage::with_recording_footprint(snap);
    let ledger_entry = storage.get(&LedgerKey::ContractData(LedgerKeyContractData {
        contract_id: xdr::Hash(contract_id),
        key,
    }))?;

    let value = if let LedgerEntryData::ContractData(entry) = ledger_entry.data {
        entry.val
    } else {
        unreachable!();
    };

    Ok(json!({
        "xdr": value.to_xdr_base64()?,
        "lastModifiedLedgerSeq": ledger_entry.last_modified_ledger_seq,
        // TODO: Find "real" ledger seq number here
        "latestLedger": 1,
    }))
}

fn parse_transaction(txn_xdr: &str, passphrase: &str) -> Result<([u8; 32], Vec<ScVal>), Error> {
    // Parse and validate the txn
    let transaction = TransactionEnvelope::from_xdr_base64(txn_xdr.to_string())?;
    let hash = hash_transaction_in_envelope(&transaction, passphrase)?;
    let ops = match transaction {
        TransactionEnvelope::TxV0(envelope) => envelope.tx.operations,
        TransactionEnvelope::Tx(envelope) => envelope.tx.operations,
        TransactionEnvelope::TxFeeBump(envelope) => {
            let FeeBumpTransactionInnerTx::Tx(tx_envelope) = envelope.tx.inner_tx;
            tx_envelope.tx.operations
        }
    };
    if ops.len() != 1 {
        return Err(Error::Xdr(XdrError::Invalid));
    }
    let op = ops.first().ok_or(Error::Xdr(XdrError::Invalid))?;
    let body = if let OperationBody::InvokeHostFunction(b) = &op.body {
        b
    } else {
        return Err(Error::Xdr(XdrError::Invalid));
    };

    if body.function != HostFunction::Call {
        return Err(Error::Xdr(XdrError::Invalid));
    };

    if body.parameters.len() < 2 {
        return Err(Error::Xdr(XdrError::Invalid));
    };

    let contract_xdr = body
        .parameters
        .get(0)
        .ok_or(Error::Xdr(XdrError::Invalid))?;
    let method_xdr = body
        .parameters
        .get(1)
        .ok_or(Error::Xdr(XdrError::Invalid))?;
    let (_, params) = body.parameters.split_at(2);

    let contract_id: [u8; 32] = if let ScVal::Object(Some(ScObject::Bytes(bytes))) = contract_xdr {
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| Error::Xdr(XdrError::Invalid))?
    } else {
        return Err(Error::Xdr(XdrError::Invalid));
    };

    // TODO: Figure out and enforce the expected type here. For now, handle both a symbol and a
    // binary. The cap says binary, but other implementations use symbol.
    let method: String = if let ScVal::Object(Some(ScObject::Bytes(bytes))) = method_xdr {
        bytes
            .try_into()
            .map_err(|_| Error::Xdr(XdrError::Invalid))?
    } else if let ScVal::Symbol(bytes) = method_xdr {
        bytes
            .try_into()
            .map_err(|_| Error::Xdr(XdrError::Invalid))?
    } else {
        return Err(Error::Xdr(XdrError::Invalid));
    };

    let mut complete_args = vec![
        ScVal::Object(Some(ScObject::Bytes(contract_id.try_into()?))),
        ScVal::Symbol(method.try_into()?),
    ];
    complete_args.extend_from_slice(params);

    Ok((hash, complete_args))
}

fn execute_transaction(
    args: &Vec<ScVal>,
    ledger_file: &PathBuf,
    commit: bool,
) -> Result<Value, Error> {
    // Initialize storage and host
    let ledger_entries = snapshot::read(ledger_file)?;

    let snap = Rc::new(snapshot::Snap {
        ledger_entries: ledger_entries.clone(),
    });
    let storage = Storage::with_recording_footprint(snap);
    let h = Host::with_storage_and_budget(storage, Budget::default());

    // TODO: Check the parameters match the contract spec, or return a helpful error message

    let res = h.invoke_function(HostFunction::Call, args.try_into()?)?;

    let (storage, budget, _) = h.try_finish().map_err(|_h| {
        HostError::from(ScStatus::HostStorageError(
            ScHostStorageErrorCode::UnknownError,
        ))
    })?;

    // Calculate the budget usage
    let mut cost = serde_json::Map::new();
    cost.insert(
        "cpuInsns".to_string(),
        Value::String(budget.get_cpu_insns_count().to_string()),
    );
    cost.insert(
        "memBytes".to_string(),
        Value::String(budget.get_mem_bytes_count().to_string()),
    );
    // TODO: Include these extra costs. Figure out the rust type conversions.
    // for cost_type in CostType::variants() {
    //     m.insert(cost_type, b.get_input(*cost_type));
    // }

    // Calculate the storage footprint
    let mut read_only: Vec<String> = vec![];
    let mut read_write: Vec<String> = vec![];
    let Footprint(m) = storage.footprint;
    for (k, v) in m {
        let dest = match v {
            AccessType::ReadOnly => &mut read_only,
            AccessType::ReadWrite => &mut read_write,
        };
        dest.push(k.to_xdr_base64()?);
    }

    if commit {
        snapshot::commit(ledger_entries, &storage.map, ledger_file)?;
    }

    Ok(json!({
        "cost": cost,
        "footprint": {
            "readOnly": read_only,
            "readWrite": read_write,
        },
        "results": vec![
            json!({ "xdr": res.to_xdr_base64()? })
        ],
        // TODO: Find "real" ledger seq number here
        "latestLedger": 1,
    }))
}

fn hash_transaction_in_envelope(
    envelope: &TransactionEnvelope,
    passphrase: &str,
) -> Result<[u8; 32], Error> {
    let tagged_transaction = match envelope {
        TransactionEnvelope::TxV0(envelope) => {
            xdr::TransactionSignaturePayloadTaggedTransaction::Tx(xdr::Transaction {
                source_account: xdr::MuxedAccount::Ed25519(
                    envelope.tx.source_account_ed25519.clone(),
                ),
                fee: envelope.tx.fee,
                seq_num: envelope.tx.seq_num.clone(),
                cond: match envelope.tx.time_bounds.clone() {
                    None => xdr::Preconditions::None,
                    Some(time_bounds) => xdr::Preconditions::Time(time_bounds),
                },
                memo: envelope.tx.memo.clone(),
                operations: envelope.tx.operations.clone(),
                ext: xdr::TransactionExt::V0,
            })
        }
        TransactionEnvelope::Tx(envelope) => {
            xdr::TransactionSignaturePayloadTaggedTransaction::Tx(envelope.tx.clone())
        }
        TransactionEnvelope::TxFeeBump(envelope) => {
            xdr::TransactionSignaturePayloadTaggedTransaction::TxFeeBump(envelope.tx.clone())
        }
    };

    // trim spaces from passphrase
    // Check if network passpharse is empty

    let network_id = xdr::Hash(hash_bytes(passphrase.as_bytes().to_vec()));
    let payload = xdr::TransactionSignaturePayload {
        network_id,
        tagged_transaction,
    };
    let tx_bytes = payload.to_xdr()?;

    // hash it
    Ok(hash_bytes(tx_bytes))
}

fn hash_bytes(b: Vec<u8>) -> [u8; 32] {
    let mut output: [u8; 32] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ];
    let mut hasher = Sha256::new();
    hasher.update(b);
    output.copy_from_slice(&hasher.finalize());
    output
}
