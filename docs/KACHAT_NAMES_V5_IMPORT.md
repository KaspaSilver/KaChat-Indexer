# `.kachat` registry v5: the migration `import` entry (2026-10-09)

**For:** the indexer session. **From:** the kachat-domains session.

**Status:** v5 is built and tested in kachat-domains but **not deployed**. A testnet drill is
coming: a new v5 registry that imports every name of the live day-clock v4 registry
(`e6b72448…7f0d`). The spec is kachat-domains `docs/REGISTRY_V5.md`.

## What the follower needs

1. **Accept v5 manifests.**
   - `registryVersion: 5` and a `params.migration` block: `predecessorRegistryId`, `snapshot`
     (file), `root`, `deadlineMs`, `sponsor`.
   - A top-level `snapshot` object: `{file, sha256, predecessorRegistryId, atMs, names}`.
   - The gap template is new (v5 gap). The name and offer templates and every other entry are
     v4's. The `register`, `merge` and `absorbed` dispatch tags are unchanged.
2. **Decode the gap's new entry `import`**, dispatch tag **`aa4cc365`**.
   - **Signature-script arguments, in ABI order:**

     | # | Argument | Type |
     |---|---|---|
     | 0 | `name` | `byte[]` |
     | 1 | `owner` | `byte[32]` |
     | 2 | `periodStart` | int |
     | 3 | `expiresAt` | int |
     | 4 | `index` | int |
     | 5 | `proof` | `byte[]`, 640 B |
     | 6 | `bySponsor` | bool |
     | 7 | `authSig` | sig |
     | 8 | `namePrefix` | `byte[]` |
     | 9 | `nameSuffix` | `byte[]` |

   - **Outputs: exactly as `register`.** 0 = gap (lo, key), 1 = gap (key, hi), 2 = the name
     `(key, padded name, owner, price 0, periodStart, expiresAt)` = bond. The owner and dates
     come from arguments 1-3.
   - **Nothing to verify off chain.** The script already checked the Merkle proof and the
     signature, so decode it like a register with those values.
   - **History:** record it as op `import` (payload `kchat:1:name:import:<name>`).
3. **`register` is closed until `migration.deadlineMs`.** No indexer change is needed: the
   contract refuses earlier registrations. `/names/status` could expose the deadline so the app
   can show "registration opens at …".
4. **Please also serve `GET /names/all`** (`docs/KACHAT_NAMES_ALL.md`). The drill checks the v5
   registry with `kachat-names verify --live`, and with `/names/all` it can prove the indexer
   too.

## When

Not yet. The v5 testnet genesis needs the owner's "send it". This note will be followed by the
deployed manifest and registry id, as with the day-clock switch
(`docs/KACHAT_NAMES_DAY_CLOCK.md`).
