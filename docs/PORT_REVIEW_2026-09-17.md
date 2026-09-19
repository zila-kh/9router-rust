# Rust port stability review — 2026-09-17

Reviewed base commit: `ae95cf25ec982c1855505d774c1b12c21b4351d1`.
The corrections described below are local working-tree changes, not a published
release. This was a focused review of the recent native search, video, and model
work, outbound request handling, and launcher checks; it was not an exhaustive
audit of every provider or dashboard feature.

## Corrected findings

1. **High: search redirects replayed credentials to another origin.**
   `ssrf_guard::fetch_public` copied the original headers and body on every
   redirect. A provider redirect to another public origin could therefore
   disclose its API key or submitted content. Redirects now require the same
   origin and unchanged URL credentials. Tests cover host changes, scheme
   downgrades, port changes, embedded credentials, and permitted same-origin
   redirects. This is an intentional security deviation from pinned upstream.
2. **Medium: malformed citation offsets could overflow.**
   Antigravity search context extraction added/subtracted a context window from
   untrusted integer offsets. Extreme JSON numbers could panic in debug builds
   or wrap in release builds. Saturating arithmetic now handles these values;
   regression tests cover both signed integer extremes and a large JSON float.
3. **High: incomplete video bodies could be reported as successful.**
   After receiving a success status, the video proxy discarded body-read errors
   and returned an empty success body. It now returns 502 for a failed body read
   or 504 for a timeout, retaining credential redaction. A local TCP regression
   server sends a 200 response with a truncated body and verifies a 502 outcome.
   This does not add automatic retries of video creation requests.
4. **Medium: Bash startup failed from the Windows checkout.**
   CRLF shell scripts failed at `set -Eeuo pipefail` under WSL. Shell scripts were
   normalized to LF and `.gitattributes` now requires LF for `*.sh`. The storage
   suite then passed under WSL. The Python suite's Bash subprocess fixtures must
   run inside Linux/WSL, rather than pass Windows paths to WSL Bash.

## Remaining release risks

- **DNS validation is separate from connection resolution.** The SSRF helper
  checks addresses with `tokio::net::lookup_host`, then lets reqwest resolve the
  hostname again. A changing DNS answer could evade the check. DNS failure also
  falls through to the request. Validation should be enforced on the actual
  connection addresses, with tests for rebinding and proxy behavior. This
  architectural issue remains unresolved; no rebinding exploit was run.
- Native route-file coverage remains 147/156 (94.2%), with explicitly recorded
  semantic gaps. Account cooldown persistence, some token refresh/retry paths,
  interactive OAuth/SSO, and incremental cross-format streaming remain release
  limitations; route coverage does not establish provider reliability.
- Real provider accounts, refresh expiry, quotas, long-running media jobs,
  restart persistence, and an actual HTTPS reverse proxy still need acceptance
  testing. No paid provider calls or production-data mutations were performed.
- Green CI on the base commit does not validate these local changes. Required
  workflows must run on the eventual committed candidate before publication.

## Validation

- Baseline: 206 Rust tests passed.
- After corrections: 209 Rust tests passed, including three new regressions.
- Release boundaries: 29 shared path fixtures, guard permissions, and seven
  safe-destination cases passed.
- Storage: 12 tests passed under WSL after line-ending correction. The initial
  native-Windows run failed due to incompatible Bash path handling; the initial
  WSL rerun reproduced the CRLF startup defect.
- Static Rust delimiter audit passed.
- Rustfmt check, Clippy with warnings denied, and `git diff --check` passed.

Verdict: the focused fixes improve reliability, but the remaining security and
provider acceptance work prevents a production stability sign-off.

## Follow-up — 2026-09-19

The DNS validation/connection race above is addressed in the current working
candidate. Guarded search and web-fetch requests now create a direct client per
hop, pin the hostname to the complete set of validated public addresses, reject
DNS failures or mixed public/private answers, and repeat the process for every
same-origin redirect. Ambient HTTP proxies are deliberately bypassed for these
user-influenced fetches because proxy-side DNS would reintroduce an unchecked
resolution step.

The remaining provider-account, real-provider, HTTPS reverse-proxy, platform,
and native-parity acceptance risks still apply.
