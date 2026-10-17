# Cursor Agent CLI port: maintenance notes

This fork (`kzu/grok-build`) carries Cursor subscription support on top of
upstream `xai-org/grok-build`. Inference can be routed through the Cursor
Agent CLI (`agent --mode ask`), while Grok keeps owning tools. The port adds
`cursor/<id>` models, `/login cursor`, and the Cursor `/usage` block.

These notes are for whoever (usually an agent) reapplies the port after
upstream moves.

## Invariants

- **One commit.** All Cursor work lives in a single commit,
  "Add Cursor subscription support via Agent CLI bridge", on branch
  `cursor-cli`, directly on top of `upstream/main`. Amend fixes into it
  (`git commit -a --amend --no-edit`). Don't stack follow-up commits.
- **Don't touch unrelated upstream code.** Only edit lines that the port
  needs. If upstream itself is broken, leave it broken and say so (see
  "Known upstream breakage").
- **No crate-wide `cargo fmt`.** It reformats upstream code and makes every
  future rebase noisy. Only run `rustfmt` on files that the port adds, or on
  shared files where `rustfmt --check` shows diffs only inside port hunks.
- **Keep the paywall policy narrow.** Suppress SuperGrok paywalls and upsells
  only while a Cursor-billed model is active, or with an explicit
  `GROK_CURSOR_CLI=1` or `[features].cursor_cli = true` override. Having
  `agent` installed is not enough.

## Out of scope

These came from the original fork (`matfk/grok-build`), were deliberately
left out, and should stay out:

- README rewrites and fork branding.
- Tier-gate lifting and "always suppress SuperGrok paywalls".
- Update-tip and updater changes.
- DeepSeek/OpenRouter BYOK and provider labels.

The included fork commits were `fb53cdeb..b2fcf68a` (Cursor-only commits).

## Rebasing onto a new upstream

```sh
git fetch upstream
git switch main && git merge --ff-only upstream/main   # main mirrors upstream
git switch cursor-cli
git rebase upstream/main
```

If the conflicts are small, resolve them in place. If upstream refactored
heavily, a mechanical rebase can go badly wrong. A previous fork merge kept
stale monolithic copies of files and doubled their size. In that case,
rebuild the patch:

```sh
git diff <old-upstream-base> cursor-cli > /tmp/cursor.patch
git switch -C cursor-cli upstream/main
git apply --3way /tmp/cursor.patch     # then resolve each conflict by hand
```

When a hunk's target file was split or removed, find where the logic moved
(see the map below) and port the hunk by hand. Don't resurrect the old file.

After resolving, always do a full verify (below) before amending.

## Where the port lives

New files (owned by the port, so conflicts here are rare):

- `crates/codegen/xai-grok-sampler/src/cursor_cli/{mod,process,prompt,stream}.rs`:
  spawns `agent`, translates prompts, parses NDJSON, and recovers tool calls.
- `crates/codegen/xai-grok-shell/src/agent/cursor_cli.rs`: model discovery
  and cache, catalog keys, the paywall policy, and `agent about` account info.

Hooks in shared upstream files (the conflict hotspots):

| Area | Location |
| --- | --- |
| Sampler backend dispatch | `xai-grok-sampler/src/{client.rs,config.rs,actor/request_task.rs,lib.rs}` |
| Backend enum / types | `xai-grok-sampling-types/src/types.rs` |
| Cursor catalog merge | `agent/remote_config/resolution.rs::resolve_model_catalog` |
| `apiBackend` model meta tag | `agent/config.rs::acp_model_meta` |
| Auth method `cursor.cli` | `agent/auth_method.rs`, `agent/mvp_agent/*` |
| Session resume id | `session/acp_session.rs` (`cursor_cli_session_id`) and `acp_session_impl/*` |
| Compaction | `session/helpers/session_compact.rs`: the `CursorCli` arm returns a deterministic "unsupported" error |
| Paywall / tier gate | `xai-grok-pager/src/app/app_view.rs` (`has_access`, `active_model_id_for_paywall`, `apply_tier_restrictions`, welcome promo filtering) |
| Promo suppression | `xai-grok-pager/src/views/announcements.rs` |
| `/login cursor` | `slash/commands/login.rs`, `app/dispatch/auth.rs`, `app/actions.rs` (`Action::LoginCursor`) |
| `/usage` Cursor block | `slash/commands/usage.rs`, `app/effects/mod.rs`, `app/dispatch/task_result.rs`, `app/status_blocks.rs` |
| Docs | `xai-grok-pager/docs/user-guide/{02-authentication,11-custom-models}.md` |

Upstream conventions the port has to follow (they've broken the port before):

- Test `SamplerConfig` literals use `..Default::default()`, so new Cursor
  fields go into `SamplerConfig::default()`.
- Test session-actor literals list every field. When the port adds one
  (such as `cursor_cli_session_id`), update every literal: `support.rs`, the
  `*_flow_tests.rs` files, and so on.
- Exhaustive `match`es on the backend enum need a `CursorCli` arm, including
  in tests (`unreachable!` is fine there).
- Side-call sampling (recap, memory dream) goes through
  `SamplingClient::conversation_collect_with_idle_timeout`, which already has
  a `CursorCli` arm.
- `/login` is a `slash_meta!` entry. It's visible when account login is
  allowed **or** `cursor_agent_available()` is true. Bare `/login` on a
  Cursor model routes to Cursor login before the `AccountLogin` refusal.
- `status.rs` keeps upstream's `push_and_page_flip`. Don't regress it to
  `push_block`.
- When upstream reshapes a message or struct (for example
  `TaskResult::SwitchModelComplete` moving to `choice: ModelChoice`), update
  the port's tests to the new shape. Don't adapt upstream to the old one.

## Lint rules that bite

- `xai-grok-sampler` has `#![deny(clippy::indexing_slicing)]`. In the
  byte-scanning parsers in `cursor_cli/prompt.rs`, use `bytes.get(i)`,
  `while let Some(&c) = ...`, and `is_some_and(...)` instead of `bytes[i]`.
  (`&str` range slicing isn't flagged.)
- `clippy.toml` bans `tokio::process::Command::spawn`. The one spawn in
  `cursor_cli/process.rs` carries
  `#[allow(clippy::disallowed_methods)] // <reason>`, because the child is
  waited on by the stream and killed on drop. Keep the reason short enough
  that rustfmt leaves it on the same line.
- No process-global mutable state that tests can observe across threads.
  `announcements.rs` keeps the published paywall model in a `Mutex` in
  production but a `thread_local!` under `cfg(test)`, the same pattern as
  `views/dock/mod.rs`. Tests suppress promos by calling
  `set_paywall_active_model(Some("cursor/…"))` and reset it at the end, not
  by setting `GROK_CURSOR_CLI`. If these come back as process globals, the
  upstream promo tests become flaky.

## Build and verify (WSL)

Native Windows builds fail in the `cc` build script, which has nothing to do
with the port. Use WSL Ubuntu, and preferably a native-filesystem clone,
because `/mnt/d` is slow and its reads were occasionally stale right after a
Windows-side edit.

```sh
export PROTOC=$HOME/.local/protoc/bin/protoc     # protoc 29.3
# Native clone kept in sync with the Windows checkout:
cd ~/grok-cursor-check-2e5f46c5
git fetch origin cursor-cli && git checkout --detach FETCH_HEAD
# For uncommitted work, sync with: (cd /mnt/d/oss/grok/grok && git diff) | git apply
# Don't cp files across: the Windows checkout is CRLF (core.autocrlf=true).

cargo check -p xai-grok-sampler -p xai-grok-shell -p xai-grok-pager \
  -p xai-grok-pager-bin --tests --message-format short

cargo clippy -p xai-grok-sampler -p xai-grok-shell -p xai-grok-pager \
  --lib --tests --message-format short
# Only findings in port-touched files matter:
git diff --name-only upstream/main HEAD > /tmp/portfiles
# ...then: rg -F -f /tmp/portfiles <clippy output>

cargo test -p xai-grok-sampler cursor_cli
cargo test -p xai-grok-shell --lib cursor
for f in cursor dispatch::tests::billing status_blocks slash::commands::usage \
         dispatch::tests::task_result views::announcements; do
  cargo test -p xai-grok-pager --lib "$f"
done
```

Run `views::announcements` several times in a row, not just once. Shared
state races only show up intermittently. Running the full
`cargo test -p xai-grok-pager --lib` once is a good catch-all.

After running `rustfmt` from WSL on the Windows checkout, files get LF line
endings. Once the commit is amended, restore CRLF with
`rm <files> && git checkout -- <files>`.

## Known upstream breakage (as of upstream `2bdd1d6a`)

These fail on plain upstream. Don't fix them in the port commit. Recheck them
after each sync, because upstream may fix them:

- `xai-grok-shell` lib tests don't compile:
  `session/acp_session_tests/tool_layer_images_bridge_tests.rs` calls
  `.encode` without `use base64::Engine`. To run the shell tests anyway, add
  `use base64::Engine as _;` **only in the scratch clone**, then revert it.
- `xai-grok-pager` integration test `registered_features_are_documented`
  includes `docs/internal/*.md`, which upstream doesn't ship. Use `--lib`.
- Clippy reports an error in `prompt_offload_tests.rs` and warnings in
  `unified_list/mod.rs` and `session_startup.rs`, all upstream.
- In WSL with snap-installed cargo, `status_line::command` tests,
  `diagnostics::fix` shell-alias tests, and a `doctor_cmd` test fail because
  of the environment (script spawn returns `Permission denied`).

To prove an error is upstream's, show that the file is unchanged by the port
(`git diff --quiet upstream/main HEAD -- <file>`) and that the port adds
nothing it depends on, such as imports. If that's ambiguous, build plain
`upstream/main`.

## Finish

1. `cargo check` is clean except for known upstream breakage.
2. Clippy is clean in port-touched files.
3. The Cursor tests pass, with the announcement tests run repeatedly.
4. `git commit -a --amend --no-edit`, and confirm
   `git rev-list --count upstream/main..cursor-cli` is `1`.
5. Update the base hash in the commit message ("ported onto upstream ...")
   and in the "Known upstream breakage" heading if they changed.
