# ar-compress

## Purpose

Prompt compression. Three pure transforms (`lite`, `rtk`, `caveman`) that take `&str` and return text, a plan that picks which run, a budget that bounds their spend, and an eval that reports what they cost.

## Configuration

The caller builds the `plan` and `budget`. `registered()` wires the three transforms into the plan. No configuration file is read here.

## Extension points

Implement the `Transform` trait to add a transform. A transform returns its input borrowed when it declines, so the no-op path allocates nothing. `Stats` reports the savings a transform made.

## Limitations

Transforms are stateless and do no I/O, so they cannot learn from history. Compression is lossy by design, and `eval` measures the cost on a fixed corpus, not on your traffic.

## Model experience

A compressed request is still valid input for the model. The savings appear in the response metadata through `Stats`, not as a changed body shape.
