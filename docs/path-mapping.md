# Path mapping

Cloud ↔ local filename mapping (plan §7), implemented in `crates/pathmap`. OneDrive
(case-insensitive; forbids `" * : < > ? / \ |`, reserved device names, trailing
dots/spaces) and Linux (case-sensitive, permissive) disagree about valid names.
Syncing both ways without a reversible layer risks data loss.

## Reversible character codec

`to_cloud(local)` encodes a Linux name into a OneDrive-safe one; `to_local(cloud)`
decodes it back. Designed so **`to_local(to_cloud(name)) == name`** (verified in the
acceptance harness A1).

- Forbidden characters map to fullwidth look-alikes and back:
  `"`→`＂` `*`→`＊` `:`→`：` `<`→`＜` `>`→`＞` `?`→`？` `\`→`＼` `|`→`｜` `/`→`／`.
- A **trailing** run of `.`/` ` (which OneDrive strips) maps to visible markers:
  `.`→`．` (fullwidth full stop), ` `→`␣` (open box). Only the trailing run is
  encoded, so interior dots/spaces are untouched.

The codec is a bijection over names that don't already contain the replacement
characters (which essentially never occur in real names).

## Reserved-name detection

`is_reserved(name)` flags Windows device names (`CON`, `PRN`, `AUX`, `NUL`,
`COM1..9`, `LPT1..9`) and OneDrive-special prefixes, for Windows-client
compatibility. Encoding reserved names is deferred to the Windows client (Phase 3);
detection is available now.

## Persistent mapping table (authoritative)

`MappingTable` is the **authoritative** roundtrip guarantee and the backstop for the
codec's edge cases and for **case-only collisions** (Linux allows `Foo` + `foo`;
OneDrive does not). Per parent directory it remembers every cloud↔local pair:

- `assign_cloud_name(parent, local)` / `assign_local_name(parent, cloud)` — allocate
  a stable counterpart, suffixing to resolve case collisions.
- `lookup_cloud(parent, local)` / `lookup_local(parent, cloud)` — reverse lookups.

Because names are tracked per parent and persisted, a move/rename never loses the
mapping even when the codec alone would be ambiguous. Property-based tests exercise
thousands of generated names for roundtrip safety.
