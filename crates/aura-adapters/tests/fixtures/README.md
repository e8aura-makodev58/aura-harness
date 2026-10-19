# Limit-message fixtures

These files stand in for the refusal text each CLI prints when a subscription
window is spent. They are **assumed formats, not recorded output**. Replace
them with real captures during the phase 0.5 spike (see
docs/implementation.md, "Quota spike") and keep the same file names so the
tests keep pointing at the right provider.

Each file is one full output as the adapter would see it on stdout + stderr.
