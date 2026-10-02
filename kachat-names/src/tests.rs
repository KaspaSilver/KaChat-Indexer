use super::*;

fn h(s: &str) -> Vec<u8> {
    hex::decode(s).expect("hex")
}
fn h32(s: &str) -> [u8; 32] {
    h(s).try_into().expect("32 bytes")
}

// --------------------------------------------------------------- unit tests --

#[test]
fn num8_roundtrips() {
    for v in [0i64, 1, 255, 256, 600, 3_500_000_000, 1_790_000_000_000, YEAR_MS_TEST] {
        assert_eq!(num8_decode(&num8_encode(v)), v, "num8 {v}");
    }
    // Fixed 8-byte little-endian: 1 -> 01 00..00
    assert_eq!(num8_encode(1), [1, 0, 0, 0, 0, 0, 0, 0]);
}
const YEAR_MS_TEST: i64 = 31_536_000_000;

#[test]
fn p2sh_script_shape() {
    let redeem = b"hello";
    let spk = p2sh_script(redeem);
    assert_eq!(spk.len(), 35);
    assert_eq!(spk[0], 0xaa); // OP_BLAKE2B
    assert_eq!(spk[1], 0x20); // push 32
    assert_eq!(spk[34], 0x87); // OP_EQUAL
    assert_eq!(&spk[2..34], &blake2b_256(redeem));
}

#[test]
fn name_key_is_blake3() {
    assert_eq!(name_key(b"alice"), *blake3::hash(b"alice").as_bytes());
}

#[test]
fn script_numbers_decode() {
    // minimal LE sign-magnitude
    assert_eq!(decode_script_number(&[]), 0);
    assert_eq!(decode_script_number(&[0x01]), 1);
    assert_eq!(decode_script_number(&[0x7f]), 127);
    assert_eq!(decode_script_number(&[0x80, 0x00]), 128);
    assert_eq!(decode_script_number(&[0xff, 0x00]), 255);
    assert_eq!(decode_script_number(&[0x81]), -1);
}

#[test]
fn state_roundtrip() {
    let gap = GapState { lo: [0u8; 32], hi: [0xff; 32] };
    assert_eq!(GapState::decode(&gap.encode()), Some(gap));

    let name = NameState {
        key: name_key(b"alice"),
        name: pad_name(b"alice"),
        owner: [7u8; 32],
        price: 500_000_000,
        expires_at: 1_822_000_000_000,
    };
    let dec = NameState::decode(&name.encode()).unwrap();
    assert_eq!(dec, name);
    assert_eq!(dec.name_str(), "alice");

    let offer = OfferState { key: name_key(b"alice"), buyer: [9u8; 32], refund_after: 600_100_000 };
    assert_eq!(OfferState::decode(&offer.encode()), Some(offer));
}

#[test]
fn sig_script_splits_args_tag_redeem() {
    // <arg: OP_2> <4-byte tag> <PUSHDATA redeem>
    let tag = [0x79u8, 0x4d, 0xca, 0x54]; // name transfer
    let redeem = vec![0x6bu8; 200]; // dummy redeem (PUSHDATA2 path)
    let mut script = vec![0x52u8]; // OP_2 (an int arg)
    script.push(0x04); // push 4 bytes
    script.extend_from_slice(&tag);
    // redeem via PUSHDATA2
    script.push(0x4d);
    script.extend_from_slice(&(redeem.len() as u16).to_le_bytes());
    script.extend_from_slice(&redeem);

    let s = decode_sig_script(&script).expect("decodes");
    assert_eq!(s.dispatch_tag, tag);
    assert_eq!(s.redeem, redeem);
    assert_eq!(s.args.len(), 1);
    assert_eq!(s.args[0].as_i64(), Some(2));
}

#[test]
fn name_rules() {
    assert!(is_valid_name(b"alice"));
    assert!(!is_valid_name(b""));
    assert!(!is_valid_name(b"-a"));
    assert!(!is_valid_name(b"a-"));
    assert!(!is_valid_name(b"Alice"));
}

// ----------------------------------------------- vectors cross-check (opt-in) --
//
// Reads the kachat-domains `kachat-names-vectors` output and checks the codec against
// it field-by-field. Skipped (not failed) when the file isn't present, so the suite
// runs anywhere; CI/dev point KACHAT_NAMES_VECTORS at a generated file.

/// The real deployed testnet-10 manifest (kachat-domains, committed — not generated).
fn manifest() -> Option<serde_json::Value> {
    let path = std::env::var("KACHAT_NAMES_MANIFEST_FILE").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../kachat-domains/manifests/kachat-names-testnet-10.json").to_string()
    });
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Authoritative cross-check against the **live chain**: `p2sh(prefix ‖ genesis-gap-state
/// ‖ suffix)` must byte-for-byte equal the genesis gap output deployed on testnet-10.
/// This validates p2sh_script (blake2b), the state encoding, and the redeem assembly
/// against real on-chain bytes — no silverc needed.
#[test]
fn reproduces_live_testnet_genesis_gap_output() {
    let Some(m) = manifest() else {
        eprintln!("skipping: no live manifest");
        return;
    };
    let gap = &m["artifacts"]["KachatGap"];
    let prefix = h(gap["prefixHex"].as_str().unwrap());
    let suffix = h(gap["suffixHex"].as_str().unwrap());

    let ao = m["genesis"]["authorizedOutputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["contract"] == "KachatGap" && o["index"] == 0)
        .expect("genesis gap output");
    let lo = h32(ao["state"]["lo"].as_str().unwrap());
    let hi = h32(ao["state"]["hi"].as_str().unwrap());

    let state = GapState { lo, hi }.encode();
    assert_eq!(state.len(), gap["stateSpan"]["len"].as_u64().unwrap() as usize, "gap state len");

    let redeem = [prefix.as_slice(), &state, &suffix].concat();
    assert_eq!(
        hex::encode(p2sh_script(&redeem)),
        ao["scriptPublicKey"].as_str().unwrap(),
        "p2sh(prefix || genesis gap state || suffix) must equal the deployed genesis output"
    );
    assert_eq!(GapState::decode(&state), Some(GapState { lo, hi }));
    eprintln!("live genesis gap output reproduced");
}

fn vectors() -> Option<serde_json::Value> {
    let path = std::env::var("KACHAT_NAMES_VECTORS")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../KaChat/KaChatTests/KachatNamesVectors.json").to_string());
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[test]
fn matches_generated_vectors() {
    let Some(v) = vectors() else {
        eprintln!("skipping: no KACHAT_NAMES_VECTORS file");
        return;
    };
    let c = &v["codecs"];

    // name keys + padding
    for e in c["nameKeys"].as_array().unwrap() {
        let name = e["name"].as_str().unwrap().as_bytes();
        assert_eq!(hex::encode(name_key(name)), e["key"].as_str().unwrap(), "key {:?}", e["name"]);
        assert_eq!(hex::encode(pad_name(name)), e["padded"].as_str().unwrap(), "pad {:?}", e["name"]);
    }

    // raw blake3 (input[i] = (i*7)%256)
    for e in c["blake3"].as_array().unwrap() {
        let n = e["len"].as_u64().unwrap() as usize;
        let input: Vec<u8> = (0..n).map(|i| ((i * 7) % 256) as u8).collect();
        assert_eq!(hex::encode(blake3::hash(&input).as_bytes()), e["hash"].as_str().unwrap(), "blake3 len {n}");
    }

    // num8
    for e in c["num8"].as_array().unwrap() {
        let val: i64 = e["value"].as_str().unwrap().parse().unwrap();
        if val == i64::MIN {
            continue;
        }
        assert_eq!(hex::encode(num8_encode(val)), e["num8"].as_str().unwrap(), "num8 {val}");
    }

    // script numbers: the push decodes back to the value
    for e in c["scriptNumbers"].as_array().unwrap() {
        let val: i64 = e["value"].as_str().unwrap().parse().unwrap();
        let pushes = parse_pushes(&h(e["push"].as_str().unwrap())).expect("push parses");
        assert_eq!(pushes.len(), 1, "one token for {val}");
        assert_eq!(pushes[0].as_i64(), Some(val), "scriptnum {val}");
    }

    // data pushes: a single token whose bytes match (empty data encodes as OP_0 / Num(0))
    for e in c["pushes"].as_array().unwrap() {
        let data = h(e["data"].as_str().unwrap());
        let pushes = parse_pushes(&h(e["push"].as_str().unwrap())).expect("push parses");
        assert_eq!(pushes.len(), 1, "one token for push {:?}", e["data"]);
        match &pushes[0] {
            Push::Data(d) => assert_eq!(d, &data, "push data"),
            Push::Num(0) => assert!(data.is_empty(), "OP_0 only for empty data"),
            // Minimal encoding: a single byte 0x01..=0x10 is OP_1..OP_16, 0x81 is OP_1NEGATE.
            Push::Num(-1) => assert_eq!(data, vec![0x81], "OP_1NEGATE only for 0x81"),
            Push::Num(n @ 1..=16) => assert_eq!(data, vec![*n as u8], "OP_{n} for byte {n}"),
            other => panic!("unexpected push {other:?}"),
        }
    }

    // commitments: commitment + redeem + P2SH spk
    for e in c["commitments"].as_array().unwrap() {
        let name = e["name"].as_str().unwrap().as_bytes();
        let owner = h32(e["owner"].as_str().unwrap());
        let salt = h32(e["salt"].as_str().unwrap());
        let com = commitment(name, &owner, &salt);
        assert_eq!(hex::encode(com), e["commitment"].as_str().unwrap(), "commitment");
        let redeem = commit_redeem(&com, &owner);
        assert_eq!(hex::encode(&redeem), e["redeem"].as_str().unwrap(), "commit redeem");
        assert_eq!(hex::encode(p2sh_script(&redeem)), e["spk"].as_str().unwrap(), "commit spk");
    }

    // states: decode matches the declared fields
    let st = &c["states"];
    let gap = GapState::decode(&h(st["gap"]["state"].as_str().unwrap())).unwrap();
    assert_eq!(hex::encode(gap.lo), st["gap"]["lo"].as_str().unwrap());
    assert_eq!(hex::encode(gap.hi), st["gap"]["hi"].as_str().unwrap());

    let name = NameState::decode(&h(st["name"]["state"].as_str().unwrap())).unwrap();
    assert_eq!(name.name_str(), st["name"]["name"].as_str().unwrap());
    assert_eq!(hex::encode(name.owner), st["name"]["owner"].as_str().unwrap());
    assert_eq!(name.price, st["name"]["price"].as_i64().unwrap());
    assert_eq!(name.expires_at, st["name"]["expiresAt"].as_i64().unwrap());

    let offer = OfferState::decode(&h(st["offer"]["state"].as_str().unwrap())).unwrap();
    assert_eq!(hex::encode(offer.key), st["offer"]["key"].as_str().unwrap());
    assert_eq!(hex::encode(offer.buyer), st["offer"]["buyer"].as_str().unwrap());
    assert_eq!(offer.refund_after, st["offer"]["refundAfter"].as_i64().unwrap());

    eprintln!("vectors cross-check passed");
}
