# UI review notes

Changes in this pass:

- Switched the GUI away from a forced monospace default font. The pixel font remains limited to the `mpt.mp` wordmark.
- Increased the smallest interface text sizes slightly while preserving the existing visual hierarchy.
- Reduced the minimum window size from 980x620 to 760x600.
- Reworked the bottom player controls into two rows so seek/volume controls no longer compete horizontally with track information and transport controls.
- Increased track-row height to match the more readable typography and kept the virtual-list row-height constant in sync.
- Constrained plugin descriptions, errors, status messages and log lines to the available card width.

Build verification could not be performed in this environment because the Rust toolchain (`cargo`/`rustc`) is not installed. Please run `cargo check` and `cargo fmt --check` locally before merging.
