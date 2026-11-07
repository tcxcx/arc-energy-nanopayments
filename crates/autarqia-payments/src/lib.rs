use autarqia_primitives::{
    ando_mppi_domain_separator, ando_mppi_voucher_digest, encode_u128_word, encode_u64_word,
    keccak256, recover_eip712_signer, BatchId, Bytes32, ChannelId, EvmAddress, SettlementRail,
    SignatureBytes, TokenAtomicAmount, ARC_EIP7708_SYSTEM_SENDER, ARC_TESTNET_ANDO_MPPI_PROXY,
    ARC_TESTNET_CHAIN_ID, ARC_TESTNET_USDC_ADDRESS,
};
use autarqia_subsystem_api::{
    ChannelLedgerView, SettlementBatchItem, SettlementSubmissionReceipt,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PaymentError {
    #[error("rpc transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("json-rpc error: {0}")]
    JsonRpc(String),
    #[error("abi decode error: {0}")]
    AbiDecode(String),
    #[error("invalid voucher or authorization: {0}")]
    Validation(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawEvmLog {
    pub address: EvmAddress,
    pub topics: Vec<Bytes32>,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsdcTransferEvent {
    pub from: EvmAddress,
    pub to: EvmAddress,
    pub amount_atomic_6dec: TokenAtomicAmount,
}

pub fn filter_arc_usdc_transfer_logs(
    logs: &[RawEvmLog],
    usdc_contract: EvmAddress,
) -> Vec<UsdcTransferEvent> {
    let eip7708_sender =
        EvmAddress::from_hex(ARC_EIP7708_SYSTEM_SENDER).unwrap_or(EvmAddress::ZERO);
    let transfer_topic = keccak256(b"Transfer(address,address,uint256)");

    let mut out = Vec::new();
    for log in logs {
        if log.address == eip7708_sender || log.address != usdc_contract {
            continue;
        }
        if log.topics.len() < 3 || log.topics[0] != transfer_topic || log.data.len() < 32 {
            continue;
        }
        let mut from = [0u8; 20];
        from.copy_from_slice(&log.topics[1].0[12..32]);
        let mut to = [0u8; 20];
        to.copy_from_slice(&log.topics[2].0[12..32]);
        let mut amt_bytes = [0u8; 16];
        amt_bytes.copy_from_slice(&log.data[16..32]);
        let amount = u128::from_be_bytes(amt_bytes);
        out.push(UsdcTransferEvent {
            from: EvmAddress(from),
            to: EvmAddress(to),
            amount_atomic_6dec: TokenAtomicAmount::new(amount),
        });
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnchainChannelRecord {
    pub finalized: bool,
    pub close_requested_at: u64,
    pub settled: TokenAtomicAmount,
    pub deposit: TokenAtomicAmount,
    pub payer: EvmAddress,
    pub payee: EvmAddress,
    pub token: EvmAddress,
    pub authorized_signer: EvmAddress,
    pub operator: EvmAddress,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantinedBatchItem {
    pub item: SettlementBatchItem,
    pub reason: String,
}

pub fn partition_valid_settlement_batch(
    chain_id: u64,
    verifying_contract: EvmAddress,
    items: &[SettlementBatchItem],
    known_channels: &[ChannelLedgerView],
) -> (Vec<SettlementBatchItem>, Vec<QuarantinedBatchItem>) {
    let domain = ando_mppi_domain_separator(chain_id, verifying_contract);
    let mut valid: Vec<SettlementBatchItem> = Vec::new();
    let mut quarantined: Vec<QuarantinedBatchItem> = Vec::new();

    for item in items {
        let Some(ch) = known_channels
            .iter()
            .find(|c| c.channel_id == item.channel_id)
        else {
            quarantined.push(QuarantinedBatchItem {
                item: item.clone(),
                reason: "unknown_channel".to_owned(),
            });
            continue;
        };

        if ch.closed {
            quarantined.push(QuarantinedBatchItem {
                item: item.clone(),
                reason: "channel_already_closed".to_owned(),
            });
            continue;
        }
        if item.cumulative_amount <= ch.settled_onchain_atomic {
            quarantined.push(QuarantinedBatchItem {
                item: item.clone(),
                reason: "nothing_new_to_settle".to_owned(),
            });
            continue;
        }
        if item.cumulative_amount > ch.deposit_atomic {
            quarantined.push(QuarantinedBatchItem {
                item: item.clone(),
                reason: "voucher_exceeds_deposit".to_owned(),
            });
            continue;
        }

        let digest = ando_mppi_voucher_digest(domain, item.channel_id, item.cumulative_amount);
        match recover_eip712_signer(digest, &item.signature) {
            Ok(signer) if signer == ch.authorized_signer => {
                if let Some(existing) = valid.iter_mut().find(|v| v.channel_id == item.channel_id) {
                    if item.cumulative_amount > existing.cumulative_amount {
                        *existing = item.clone();
                    }
                } else {
                    valid.push(item.clone());
                }
            }
            Ok(other) => quarantined.push(QuarantinedBatchItem {
                item: item.clone(),
                reason: format!("signer_mismatch:{other}"),
            }),
            Err(e) => quarantined.push(QuarantinedBatchItem {
                item: item.clone(),
                reason: format!("invalid_signature:{e}"),
            }),
        }
    }

    (valid, quarantined)
}

pub fn encode_settle_calldata(
    channel_id: ChannelId,
    cumulative_amount: TokenAtomicAmount,
    signature: &SignatureBytes,
) -> Vec<u8> {
    let selector = &keccak256(b"settle(bytes32,uint128,bytes)").0[0..4];
    let mut out = Vec::with_capacity(4 + 5 * 32);
    out.extend_from_slice(selector);
    out.extend_from_slice(&channel_id.0);
    out.extend_from_slice(&encode_u128_word(cumulative_amount.0));
    out.extend_from_slice(&encode_u64_word(96));
    out.extend_from_slice(&encode_u64_word(signature.0.len() as u64));
    out.extend_from_slice(&signature.0);
    let rem = signature.0.len() % 32;
    if rem != 0 {
        out.resize(out.len() + (32 - rem), 0);
    }
    out
}

#[derive(Clone)]
pub struct AndoMppiArcClient {
    rpc_url: String,
    chain_id: u64,
    proxy_address: EvmAddress,
    http: reqwest::Client,
}

impl AndoMppiArcClient {
    pub fn new(rpc_url: impl Into<String>, chain_id: u64, proxy_address: EvmAddress) -> Self {
        Self {
            rpc_url: rpc_url.into(),
            chain_id,
            proxy_address,
            http: reqwest::Client::new(),
        }
    }

    pub fn arc_testnet_default() -> Self {
        Self::new(
            "https://rpc.testnet.arc.network",
            ARC_TESTNET_CHAIN_ID,
            EvmAddress::from_hex(ARC_TESTNET_ANDO_MPPI_PROXY).unwrap(),
        )
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub fn proxy_address(&self) -> EvmAddress {
        self.proxy_address
    }

    pub async fn eth_call(&self, to: EvmAddress, calldata: &[u8]) -> Result<Vec<u8>, PaymentError> {
        let data_hex = format!("0x{}", hex::encode(calldata));
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_call",
            "params": [
                {
                    "to": to.to_hex(),
                    "data": data_hex,
                },
                "latest"
            ]
        });
        let resp: serde_json::Value = self
            .http
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        if let Some(err) = resp.get("error") {
            return Err(PaymentError::JsonRpc(err.to_string()));
        }
        let result_str = resp
            .get("result")
            .and_then(|v| v.as_str())
            .ok_or_else(|| PaymentError::JsonRpc("missing result in eth_call".to_owned()))?;
        let trimmed = result_str.strip_prefix("0x").unwrap_or(result_str);
        hex::decode(trimmed).map_err(|e| PaymentError::AbiDecode(e.to_string()))
    }

    pub async fn fetch_domain_separator(&self) -> Result<Bytes32, PaymentError> {
        let selector = &keccak256(b"domainSeparator()").0[0..4];
        let raw = self.eth_call(self.proxy_address, selector).await?;
        if raw.len() != 32 {
            return Err(PaymentError::AbiDecode(format!(
                "expected 32 bytes for domainSeparator, got {}",
                raw.len()
            )));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&raw);
        Ok(Bytes32(out))
    }

    pub async fn fetch_channel(
        &self,
        channel_id: ChannelId,
    ) -> Result<OnchainChannelRecord, PaymentError> {
        let selector = &keccak256(b"getChannel(bytes32)").0[0..4];
        let mut calldata = Vec::with_capacity(36);
        calldata.extend_from_slice(selector);
        calldata.extend_from_slice(&channel_id.0);
        let raw = self.eth_call(self.proxy_address, &calldata).await?;
        if raw.len() < 9 * 32 {
            return Err(PaymentError::AbiDecode(format!(
                "expected 288 bytes from getChannel, got {}",
                raw.len()
            )));
        }
        let finalized = raw[31] != 0;
        let close_requested_at = u64::from_be_bytes(raw[56..64].try_into().unwrap());
        let settled = u128::from_be_bytes(raw[80..96].try_into().unwrap());
        let deposit = u128::from_be_bytes(raw[112..128].try_into().unwrap());
        let mut payer = [0u8; 20];
        payer.copy_from_slice(&raw[140..160]);
        let mut payee = [0u8; 20];
        payee.copy_from_slice(&raw[172..192]);
        let mut token = [0u8; 20];
        token.copy_from_slice(&raw[204..224]);
        let mut authorized_signer = [0u8; 20];
        authorized_signer.copy_from_slice(&raw[236..256]);
        let mut operator = [0u8; 20];
        operator.copy_from_slice(&raw[268..288]);

        Ok(OnchainChannelRecord {
            finalized,
            close_requested_at,
            settled: TokenAtomicAmount::new(settled),
            deposit: TokenAtomicAmount::new(deposit),
            payer: EvmAddress(payer),
            payee: EvmAddress(payee),
            token: EvmAddress(token),
            authorized_signer: EvmAddress(authorized_signer),
            operator: EvmAddress(operator),
        })
    }

    pub fn build_simulated_batch_receipt(
        &self,
        batch_id: BatchId,
        rail: SettlementRail,
        valid_items: &[SettlementBatchItem],
        block_number: u64,
    ) -> SettlementSubmissionReceipt {
        let mut buf = Vec::new();
        buf.extend_from_slice(batch_id.as_str().as_bytes());
        for item in valid_items {
            buf.extend_from_slice(&item.channel_id.0);
            buf.extend_from_slice(&item.cumulative_amount.0.to_be_bytes());
        }
        let tx_hash = keccak256(&buf);
        SettlementSubmissionReceipt {
            batch_id,
            rail,
            tx_hash,
            settled_channels: valid_items
                .iter()
                .map(|i| (i.channel_id, i.cumulative_amount))
                .collect(),
            block_number,
        }
    }
}

pub fn default_arc_usdc_address() -> EvmAddress {
    EvmAddress::from_hex(ARC_TESTNET_USDC_ADDRESS).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use autarqia_primitives::{address_from_signing_key, sign_eip712_digest};
    use k256::ecdsa::SigningKey;

    #[test]
    fn filters_eip7708_duplicate_system_logs() {
        let usdc = default_arc_usdc_address();
        let eip7708 = EvmAddress::from_hex(ARC_EIP7708_SYSTEM_SENDER).unwrap();
        let transfer_topic = keccak256(b"Transfer(address,address,uint256)");
        let from_word = Bytes32(
            EvmAddress::from_hex("0x1111111111111111111111111111111111111111")
                .unwrap()
                .to_word32(),
        );
        let to_word = Bytes32(
            EvmAddress::from_hex("0x2222222222222222222222222222222222222222")
                .unwrap()
                .to_word32(),
        );

        let logs = vec![
            RawEvmLog {
                address: eip7708,
                topics: vec![transfer_topic, from_word, to_word],
                data: encode_u128_word(1_000_000_000_000_000_000).to_vec(),
            },
            RawEvmLog {
                address: usdc,
                topics: vec![transfer_topic, from_word, to_word],
                data: encode_u128_word(1_000_000).to_vec(),
            },
        ];

        let filtered = filter_arc_usdc_transfer_logs(&logs, usdc);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].amount_atomic_6dec.0, 1_000_000);
    }

    #[test]
    fn batch_partitioner_isolates_invalid_or_over_deposit_vouchers() {
        let sk = SigningKey::from_bytes((&[0x44u8; 32]).into()).unwrap();
        let signer = address_from_signing_key(&sk);
        let proxy = EvmAddress::from_hex(ARC_TESTNET_ANDO_MPPI_PROXY).unwrap();
        let domain = ando_mppi_domain_separator(ARC_TESTNET_CHAIN_ID, proxy);

        let ch_good = Bytes32::from_hex(
            "0x1111111111111111111111111111111111111111111111111111111111111111",
        )
        .unwrap();
        let ch_bad = Bytes32::from_hex(
            "0x2222222222222222222222222222222222222222222222222222222222222222",
        )
        .unwrap();

        let good_amt = TokenAtomicAmount::new(800);
        let good_sig = sign_eip712_digest(
            &sk,
            ando_mppi_voucher_digest(domain, ch_good, good_amt),
        )
        .unwrap();

        let over_amt = TokenAtomicAmount::new(9_999);
        let over_sig = sign_eip712_digest(
            &sk,
            ando_mppi_voucher_digest(domain, ch_bad, over_amt),
        )
        .unwrap();

        let channels = vec![
            ChannelLedgerView {
                channel_id: ch_good,
                rail: SettlementRail::AndoMppiArcV1,
                payer: signer,
                payee: signer,
                authorized_signer: signer,
                deposit_atomic: TokenAtomicAmount::new(1_000),
                highest_verified_voucher_atomic: good_amt,
                highest_voucher_signature: Some(good_sig.clone()),
                settled_onchain_atomic: TokenAtomicAmount::ZERO,
                accrued_cumulatively_atomic: good_amt,
                acknowledged_cumulatively_atomic: good_amt,
                close_requested_at_unix_s: None,
                closed: false,
            },
            ChannelLedgerView {
                channel_id: ch_bad,
                rail: SettlementRail::AndoMppiArcV1,
                payer: signer,
                payee: signer,
                authorized_signer: signer,
                deposit_atomic: TokenAtomicAmount::new(1_000),
                highest_verified_voucher_atomic: TokenAtomicAmount::new(500),
                highest_voucher_signature: None,
                settled_onchain_atomic: TokenAtomicAmount::ZERO,
                accrued_cumulatively_atomic: TokenAtomicAmount::new(500),
                acknowledged_cumulatively_atomic: TokenAtomicAmount::new(500),
                close_requested_at_unix_s: None,
                closed: false,
            },
        ];

        let items = vec![
            SettlementBatchItem {
                channel_id: ch_good,
                cumulative_amount: good_amt,
                signature: good_sig,
            },
            SettlementBatchItem {
                channel_id: ch_bad,
                cumulative_amount: over_amt,
                signature: over_sig,
            },
        ];

        let (valid, quarantined) =
            partition_valid_settlement_batch(ARC_TESTNET_CHAIN_ID, proxy, &items, &channels);
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].channel_id, ch_good);
        assert_eq!(quarantined.len(), 1);
        assert_eq!(quarantined[0].item.channel_id, ch_bad);
        assert_eq!(quarantined[0].reason, "voucher_exceeds_deposit");
    }
}
