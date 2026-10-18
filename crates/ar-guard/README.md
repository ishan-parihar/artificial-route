# ar-guard

## Purpose

Two-stage text guard. `redact_bidi` removes PII and credentials. `inspect` flags prompt-injection attempts. Both are single linear passes with no backtracking, so there is no ReDoS surface.

## Configuration

The enforcement mode is set by the server (warn or enforce). This crate has no settings of its own; its rules are compiled in.

## Extension points

Add a credential pattern to the redact table or an injection phrase to the inspect table. Each rule is data, not control flow, so a new rule cannot change the matching algorithm.

## Limitations

Pattern-based: it finds what its rules describe and nothing else. It is not a classifier, so a paraphrased injection can pass. The guard performs no I/O and keeps no state, so it cannot learn across requests.

## Model experience

In warn mode a flagged request is forwarded and logged with the matched rule names. Enforce mode does not forward it. The redacted text is the only text that may be logged, cached, or forwarded.
