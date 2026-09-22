# Native quote reference

This wrapper is **internal conformance research**, excluded from the default
execution/harness dependency graph. `native-reference` explicitly enables its
optional dependencies in the SVM harness.

The downloaded `riptide-amm 2.1.1/LICENSE` states that use, modification and
redistribution require the holder's written permission. Do not treat this SDK
as an open-source production dependency. No SDK source is vendored here. Resolve
the licensing terms before enabling it in a distributed product, or implement
an independently specified compatible kernel.

The default Skew engine uses its own typed instruction lowering and deployed-SBF
execution oracle. Its settlement, split and crossing evidence does not depend
on enabling the reference SDK.

The reference quote binds a decoded market to its hash, slot and execution
context. The penalty is explicit: directly applying another router's execution
terms produces a different quote. Native pricing still requires final SBF
simulation before admission.
