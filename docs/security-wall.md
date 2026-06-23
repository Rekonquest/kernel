# The Capability Security Wall

Status: **ENFORCING and BOOT-VERIFIED.** Scheme creation is authorized through the
capability wall (`scheme/mod.rs` → `security::authorize_scheme_create`); the kernel
boots to `redox login:` in QEMU with the full daemon chain up (pcid → virtio-gpud →
display), no panic — every boot-time scheme creation authorized through the wall.
Behaviour is preserved for `uid == 0`, but the authority is now a **revocable
capability** (a revoked domain is refused even at uid 0). Remaining work is migrating
the other `euid`-gated seams (`fchown`, proc ops) the same way, and tightening policy.

> Boot-test note: `WALL_CAPACITY` must stay small — the kit `HandleTable` is a
> fixed-capacity array built on the kernel stack at boot; an oversized value overflows
> the stack (double fault in `kmain`). 256 boots cleanly; 4096 did not.

The kernel's security is moving from ambient identity checks (`euid == 0`, scattered)
to **held, immutable, revocable capabilities** — composed from the `driver-primitives`
kit and driven by a kernel orchestrator. This is the primitive + orchestrator doctrine:
the kit provides dumb mechanism; the kernel owns all policy.

## What exists now (verified)

- **Primitive — `driver-primitives::authority::AuthorityWall`.** A composed capability
  table: `HandleTable` (generation-checked, unforgeable handles) + `SeqCounter` (issue
  epoch). Three invariants:
  - *Immutable by construction* — a capability's `Authority` is fixed at mint; no setter.
  - *Capability-to-construct* — authorization is *holding* a wall-minted `CapToken`; a
    forged or revoked token is rejected by the generation check, not an advisory compare.
  - *Revocation is destruction* — `revoke` bumps the slot generation, invalidating every
    outstanding copy at once.
  7 unit tests; fmt + clippy `-D warnings` + bare-metal build green in CI.
- **Orchestrator — `kernel::security`.** Owns the single wall plus a domain index
  (`BTreeMap<DomainId, CapToken>`). `init()` (called from `kmain`) grants the root domain
  `Authority::ALL`. Facade: `grant_domain`, `authorize_domain`, `revoke_domain` — identity
  is **structural** (the kernel names the domain from the acting context, never asserted).
  `make check` green; zero new warnings.

Nothing enforces yet — `authorize_domain` is wired but not consulted at a gate. That is
deliberate: enforcement cannot land until distribution lands (below), and both need a
boot test.

## The target seam

`SchemeList::kdup`, `src/scheme/mod.rs:312-325`:

```rust
Handle::SchemeCreationCapability => (),     // :312 — real capability (already structural)
// ... b"create-scheme" payload check ...   // :316
if caller.uid != 0 {                        // :323 — the ambient identity wall to replace
    return Err(Error::new(EACCES));
}
```

Possession of the scheme-creation fd is *already* a real wall; the `uid != 0` line is the
painted-on advisory check beside it. The end state replaces line 323 with:

```rust
if !security::authorize_domain(domain_of(caller), Authority::CREATE_SCHEME) {
    return Err(Error::new(EACCES));
}
```

where `domain_of(caller)` is derived structurally from the caller's context
(`owner_proc_id`, `context.rs:159`, or `pid`, `:164` — decision below).

## Why enforcement cannot land yet (the honest blocker)

`authorize_domain` denies any domain that holds no capability. Today only the root domain
is granted. If the gate flipped now, **every non-root scheme-creating daemon would be
denied and boot/driver-init would break.** Enforcement therefore requires **capability
distribution first**: the orchestrator must grant each context its authority as it is
created. And because a wrong distribution policy wedges boot, the change must be
**boot-verified in QEMU** — it is not provable by `make check` (compile only).

## Activation plan (each step boot-tested)

1. **DomainId mapping.** Fix `DomainId = owner_proc_id` (process-granular; a process is the
   natural authority domain). Resolve the bootstrap process's id and confirm `init()`'s
   `ROOT_DOMAIN` equals it (or grant the real bootstrap id). *Boot-test:* unchanged boot.
2. **Distribution on spawn.** Hook context/process creation: `security::grant_domain(child,
   policy(parent, child))`. Initial policy = inherit the parent's authority (preserves
   today's behavior exactly), so the capability graph mirrors the current process tree.
   *Boot-test:* unchanged boot; every domain that creates a scheme today now holds
   `CREATE_SCHEME`.
3. **Gate flip behind a feature.** Add cargo feature `cap-enforce-scheme-create` (default
   **off** → no behavior change → default boot safe). Under the feature, replace
   `scheme/mod.rs:323` with the `authorize_domain` check. *Boot-test with the feature on:*
   boot must still reach userspace and drivers must still create their schemes.
4. **Tighten policy.** Once inherit-all enforces cleanly, narrow it: only the domains that
   legitimately create schemes receive `CREATE_SCHEME`; everything else is denied. This is
   the actual security gain. *Boot-test:* the system still boots; an unauthorized domain is
   refused (a positive enforcement test).
5. **Revocation on teardown.** Call `revoke_domain` from the context exit path so a dead
   domain's capability is destroyed. *Boot-test:* spawn/exit cycles leak no capabilities
   (`live()` returns to baseline).
6. **Remove the ambient check.** With enforcement boot-proven, delete the `uid != 0` line
   (and pursue the other `euid`-gated seams the same way). Make the feature default-on, then
   non-optional.

## Distribution discipline (do not regress)

- Identity is **derived**, never a caller argument.
- One capability per domain; re-granting **revokes** the old one (no rights edit).
- No mutable policy state on the data path; `authorize_domain` is an O(log n) map lookup +
  the wall's O(1) generation check, under one lock — keep it off the hottest paths or move
  to a per-context cached token if it shows up in profiles.
- Never bolt an advisory parallel check beside the real fd/cap wall; the gate either is the
  capability check or it is not there.
