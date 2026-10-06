//! `.kachat` name pushes (KACHAT_NAMES_INDEXER.md Part E): which pushes a batch of registry
//! events produces, which expiry reminders are due, and the POST to the push service's
//! internal route. The two planners are pure so they are unit-tested; the follower decides
//! *when* to send (only once caught up, so historical events never notify).

use std::collections::HashMap;

use kachat_names::ingest::{Event, Outpoint, Tracked};
use kachat_names::NameState;
use serde_json::json;

pub const DAY_MS: i64 = 86_400_000;

/// One push for the push service's `/internal/push/names`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamePush {
    /// Owner/seller/buyer x-only key; the follower turns it into the recipient address.
    pub to_key: [u8; 32],
    pub event: &'static str,
    pub name: String,
    pub tx_id: String,
    pub amount: Option<i64>,
    pub days: Option<u32>,
    /// Stable per push, so a retry never notifies twice.
    pub dedup: String,
}

/// The pushes a batch's events produce. `before` is the tracked set before the batch (the
/// spent offer of an accept lives only there); `after` is the set after it; `values` holds
/// each tracked UTXO's value (offer amounts); `names` maps keys to name strings.
pub fn event_pushes(
    events: &[Event],
    before: &HashMap<Outpoint, Tracked>,
    after: &HashMap<Outpoint, Tracked>,
    values: &HashMap<Outpoint, u64>,
    names: &HashMap<[u8; 32], String>,
    tx_inputs: &HashMap<[u8; 32], Vec<Outpoint>>,
) -> Vec<NamePush> {
    let owner_of = |key: &[u8; 32], set: &HashMap<Outpoint, Tracked>| {
        set.values().find_map(|t| match t {
            Tracked::Name(n) if &n.key == key => Some(n.owner),
            _ => None,
        })
    };
    let mut out = Vec::new();
    for e in events {
        let Some(name) = names.get(&e.key).cloned() else { continue };
        let tx = hex::encode(e.tx_id);
        match e.op {
            // A new offer: tell the name's current owner, with the offered amount.
            "offer" => {
                let Some(owner) = owner_of(&e.key, after) else { continue };
                if Some(owner) == e.to {
                    continue; // an offer on your own name
                }
                let amount = after
                    .iter()
                    .find(|(op, t)| op.0 == e.tx_id && matches!(t, Tracked::Offer(o) if o.key == e.key))
                    .and_then(|(op, _)| values.get(op))
                    .map(|v| *v as i64);
                out.push(NamePush {
                    to_key: owner,
                    event: "name_offer",
                    name,
                    tx_id: tx.clone(),
                    amount,
                    days: None,
                    dedup: format!("{tx}:offer"),
                });
            }
            // A listing bought: the seller was paid the listed price.
            "sale" => {
                if let Some(seller) = e.from {
                    out.push(NamePush {
                        to_key: seller,
                        event: "name_sold",
                        name,
                        tx_id: tx.clone(),
                        amount: e.price,
                        days: None,
                        dedup: format!("{tx}:sold"),
                    });
                }
            }
            // An offer accepted: the buyer gets the name, the seller the offer's amount.
            "offer_accepted" => {
                let offer_value = tx_inputs
                    .get(&e.tx_id)
                    .into_iter()
                    .flatten()
                    .find(|op| matches!(before.get(op), Some(Tracked::Offer(o)) if o.key == e.key))
                    .and_then(|op| values.get(op))
                    .map(|v| *v as i64);
                if let Some(buyer) = e.to {
                    out.push(NamePush {
                        to_key: buyer,
                        event: "name_offer_accepted",
                        name: name.clone(),
                        tx_id: tx.clone(),
                        amount: None,
                        days: None,
                        dedup: format!("{tx}:accepted"),
                    });
                }
                if let Some(seller) = e.from {
                    out.push(NamePush {
                        to_key: seller,
                        event: "name_sold",
                        name,
                        tx_id: tx.clone(),
                        amount: offer_value,
                        days: None,
                        dedup: format!("{tx}:sold"),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// A reminder that is due now for a live name: (event, days, dedup kind).
/// Registry v2 schedule: `name_renewal_open` when renewing opens (`expiresAt − renewWindow`),
/// `name_expiring` 3 days and 1 day before, `name_grace` at `expiresAt`. Each has a window that
/// ends where the next begins, so a follower that was down sends only the current one; a
/// renewal moves `expiresAt`, which starts a fresh schedule (the dedup is per expiry).
pub fn due_reminder(n: &NameState, now: i64, renew_window_ms: i64, grace_ms: i64) -> Option<(&'static str, Option<u32>, &'static str)> {
    let e = n.expires_at;
    if now >= e + grace_ms {
        None // lapsed: reclaimable, nothing left to remind
    } else if now >= e {
        Some(("name_grace", None, "grace"))
    } else if now >= e - DAY_MS {
        Some(("name_expiring", Some(1), "expiring1"))
    } else if now >= e - 3 * DAY_MS {
        Some(("name_expiring", Some(3), "expiring3"))
    } else if now >= e - renew_window_ms {
        Some(("name_renewal_open", None, "renewal_open"))
    } else {
        None
    }
}

/// Plain-English title/body; the app's extension rewrites them in the phone's language.
pub fn text(event: &str, name: &str, amount: Option<i64>, days: Option<u32>) -> (String, String) {
    let kas = |s: i64| {
        let whole = s / 100_000_000;
        let frac = (s % 100_000_000).abs();
        if frac == 0 { format!("{whole}") } else { format!("{whole}.{:08}", frac).trim_end_matches('0').to_string() }
    };
    match event {
        "name_offer" => (
            format!("{name}.kachat: new offer"),
            amount.map(|a| format!("Someone offered {} for it.", kas(a))).unwrap_or_else(|| "Someone made an offer for it.".into()),
        ),
        "name_sold" => (format!("{name}.kachat sold"), "Your name was sold.".into()),
        "name_offer_accepted" => (format!("{name}.kachat is yours"), "Your offer was accepted.".into()),
        "name_renewal_open" => (format!("{name}.kachat can be renewed"), "Renewal is open until it expires.".into()),
        "name_expiring" => (
            format!("{name}.kachat expires in {} day{}", days.unwrap_or(1), if days == Some(1) { "" } else { "s" }),
            "Renew it to keep it.".into(),
        ),
        "name_grace" => (format!("{name}.kachat has expired"), "Renew it during the grace period to keep it.".into()),
        _ => (format!("{name}.kachat"), String::new()),
    }
}

/// POST one push to the push service (`{PUSH_INTERNAL_URL}/names`).
pub async fn send(
    http: &reqwest::Client,
    base: &str,
    secret: Option<&str>,
    to_address: &str,
    p: &NamePush,
) -> anyhow::Result<()> {
    let (title, body) = text(p.event, &p.name, p.amount, p.days);
    let mut req = http.post(format!("{}/names", base.trim_end_matches('/'))).json(&json!({
        "to_address": to_address,
        "event": p.event,
        "name": p.name,
        "tx_id": p.tx_id,
        "amount": p.amount.map(|a| a.to_string()),
        "days": p.days,
        "title": title,
        "body": body,
        "dedup": p.dedup,
    }));
    if let Some(secret) = secret {
        req = req.header("x-internal-secret", secret);
    }
    let resp = req.send().await?;
    anyhow::ensure!(resp.status().is_success(), "push service answered {}", resp.status());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kachat_names::{OfferState, pad_name};

    const NOW: i64 = 1_800_000_000_000;

    fn name(key: [u8; 32], owner: [u8; 32], price: i64, expires: i64) -> NameState {
        NameState { key, name: pad_name(b"alice"), owner, price, period_start: expires - 31_536_000_000, expires_at: expires }
    }
    fn ev(op: &'static str, key: [u8; 32], tx: u8) -> Event {
        Event { op, key, tx_id: [tx; 32], block: [0; 32], daa: 1, at: NOW, from: None, to: None, price: None, years: None }
    }

    #[test]
    fn offer_notifies_the_owner_with_the_amount() {
        let (key, owner, buyer) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        let offer_op = ([9u8; 32], 0);
        let after: HashMap<_, _> = [
            (([5u8; 32], 2), Tracked::Name(name(key, owner, 0, NOW + 400 * DAY_MS))),
            (offer_op, Tracked::Offer(OfferState { key, buyer, seller: None, refund_after: 10 })),
        ]
        .into();
        let values: HashMap<_, _> = [(offer_op, 900_000_000u64)].into();
        let names: HashMap<_, _> = [(key, "alice".to_string())].into();
        let mut e = ev("offer", key, 9);
        e.to = Some(buyer);
        let p = event_pushes(&[e], &HashMap::new(), &after, &values, &names, &HashMap::new());
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].event, p[0].to_key, p[0].amount), ("name_offer", owner, Some(900_000_000)));
        assert_eq!(text("name_offer", "alice", Some(900_000_000), None).1, "Someone offered 9 for it.");
    }

    #[test]
    fn offer_accept_tells_buyer_and_pays_seller_the_offer() {
        let (key, seller, buyer) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        let offer_op = ([8u8; 32], 0);
        let before: HashMap<_, _> = [(offer_op, Tracked::Offer(OfferState { key, buyer, seller: None, refund_after: 10 }))].into();
        let values: HashMap<_, _> = [(offer_op, 1_000_000_000u64)].into();
        let names: HashMap<_, _> = [(key, "bravo".to_string())].into();
        let inputs: HashMap<_, _> = [([7u8; 32], vec![([6u8; 32], 2), offer_op])].into();
        let mut e = ev("offer_accepted", key, 7);
        e.from = Some(seller);
        e.to = Some(buyer);
        let p = event_pushes(&[e], &before, &HashMap::new(), &values, &names, &inputs);
        assert_eq!(p.iter().map(|x| (x.event, x.to_key)).collect::<Vec<_>>(), vec![
            ("name_offer_accepted", buyer),
            ("name_sold", seller)
        ]);
        assert_eq!(p[1].amount, Some(1_000_000_000));
    }

    #[test]
    fn sale_pays_the_listed_price_and_own_offers_are_silent() {
        let (key, seller) = ([1u8; 32], [2u8; 32]);
        let names: HashMap<_, _> = [(key, "alice".to_string())].into();
        let mut sale = ev("sale", key, 4);
        sale.from = Some(seller);
        sale.price = Some(5_000_000_000);
        let p = event_pushes(&[sale], &HashMap::new(), &HashMap::new(), &HashMap::new(), &names, &HashMap::new());
        assert_eq!((p[0].event, p[0].to_key, p[0].amount), ("name_sold", seller, Some(5_000_000_000)));

        let after: HashMap<_, _> = [(([5u8; 32], 2), Tracked::Name(name(key, seller, 0, NOW + DAY_MS * 400)))].into();
        let mut own = ev("offer", key, 5);
        own.to = Some(seller);
        assert!(event_pushes(&[own], &HashMap::new(), &after, &HashMap::new(), &names, &HashMap::new()).is_empty());
    }

    #[test]
    fn reminder_schedule_follows_registry_v2() {
        let window = 10 * DAY_MS;
        let grace = 10 * DAY_MS;
        let exp = NOW;
        let at = |t: i64| due_reminder(&name([1; 32], [2; 32], 0, exp), t, window, grace).map(|r| r.2);
        assert_eq!(at(exp - 11 * DAY_MS), None, "renewing not open yet");
        assert_eq!(at(exp - 10 * DAY_MS), Some("renewal_open"));
        assert_eq!(at(exp - 3 * DAY_MS), Some("expiring3"));
        assert_eq!(at(exp - DAY_MS), Some("expiring1"));
        assert_eq!(at(exp), Some("grace"));
        assert_eq!(at(exp + grace), None, "lapsed: nothing to remind");
        assert_eq!(text("name_expiring", "alice", None, Some(3)).0, "alice.kachat expires in 3 days");
    }
}
