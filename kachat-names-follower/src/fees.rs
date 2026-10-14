//! What .kachat transactions paid miners, for history rows indexed before the follower recorded
//! it (`names_history.fee`, new rows get it from the node's spent amounts as they are indexed).
//!
//! A background task asks the Kaspa REST API for each such transaction with its previous
//! outpoints resolved, and stores inputs minus outputs: the network fee plus the registry's
//! price, which is paid as fee. A transaction the API cannot account for gets -1, so it is not
//! asked about again and is left out of the totals. Only where the REST API is allowed
//! (KACHAT_NAMES_REST_BOOTSTRAP is not `off`).
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;
use sqlx::PgPool;
use tracing::{info, warn};

const BATCH: i64 = 20;

/// Runs for the life of the follower: a batch every couple of seconds while rows are missing a
/// fee, then a look every ten minutes.
pub async fn backfill(pool: PgPool, http: reqwest::Client, base: String) {
    let mut total = 0usize;
    loop {
        let pause = match step(&pool, &http, &base).await {
            Ok(0) => {
                if total > 0 {
                    info!("[names] fees: filled in {total} older transaction(s) from {base}");
                    total = 0;
                }
                600
            }
            Ok(n) => {
                total += n;
                2
            }
            Err(e) => {
                warn!("[names] fees: {e}; trying again in 5 minutes");
                300
            }
        };
        tokio::time::sleep(Duration::from_secs(pause)).await;
    }
}

/// One batch; how many transactions got an answer (0 = nothing left, or the API is not answering).
async fn step(pool: &PgPool, http: &reqwest::Client, base: &str) -> Result<usize> {
    let ids: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT DISTINCT tx_id FROM names_history WHERE fee IS NULL LIMIT $1").bind(BATCH).fetch_all(pool).await?;
    let mut answered = 0;
    for id in ids {
        let url = format!(
            "{base}/transactions/{}?inputs=true&outputs=true&resolve_previous_outpoints=light",
            hex::encode(&id)
        );
        let fee = match http.get(&url).send().await {
            Ok(r) if r.status().is_success() => fee_of(&r.json::<Value>().await?).unwrap_or(-1),
            Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => -1,
            _ => continue, // the API is busy or down: this one is asked again later
        };
        sqlx::query("UPDATE names_history SET fee = $1 WHERE tx_id = $2 AND fee IS NULL")
            .bind(fee)
            .bind(&id)
            .execute(pool)
            .await?;
        answered += 1;
    }
    Ok(answered)
}

fn amount(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

/// Inputs minus outputs, in sompi; None unless every input's amount is there.
pub fn fee_of(tx: &Value) -> Option<i64> {
    let mut spent: u64 = 0;
    for i in tx["inputs"].as_array()? {
        spent = spent.checked_add(amount(&i["previous_outpoint_amount"])?)?;
    }
    let mut out: u64 = 0;
    for o in tx["outputs"].as_array()? {
        out = out.checked_add(amount(&o["amount"])?)?;
    }
    i64::try_from(spent.checked_sub(out)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registration_pays_its_price_and_the_network_fee() {
        // testnet-10 tx 0847d5c9…: a .kachat registration (amounts as the REST API serves them).
        let tx = serde_json::json!({
            "inputs": [{"previous_outpoint_amount": 100000000}, {"previous_outpoint_amount": "996685271298"},
                       {"previous_outpoint_amount": 100000000}],
            "outputs": [{"amount": 100000000}, {"amount": 100000000}, {"amount": 100000000}, {"amount": "991583689898"}],
        });
        assert_eq!(fee_of(&tx), Some(5_001_581_400));
    }

    #[test]
    fn unknown_input_amounts_give_no_fee() {
        let tx = serde_json::json!({"inputs": [{"previous_outpoint_amount": null}], "outputs": [{"amount": 1}]});
        assert_eq!(fee_of(&tx), None);
        assert_eq!(fee_of(&serde_json::json!({"inputs": [{"previous_outpoint_amount": 1}], "outputs": [{"amount": 2}]})), None);
    }
}
