# Temporal polyfill

This directory vendors `@js-temporal/polyfill` 0.5.1's UMD distribution,
formatted with Prettier 3.6.2 so QuickJS can report useful source locations.

- Source: https://github.com/js-temporal/temporal-polyfill
- Package integrity: `sha512-hloP58zRVCRSp[...]wvZvmapQnKwFQ==`
- License: ISC

The QuickJS backend installs the polyfill as the engine's `Temporal` intrinsic.
Named time-zone calculations are supplied by `temporal_rs` and bundled
zoneinfo64 data through the backend's `Intl.DateTimeFormat` host implementation.

## zoneinfo64

`zoneinfo64.res.br` is the Brotli stream of `src/data/zoneinfo64.res` from
[`zoneinfo64`](https://crates.io/crates/zoneinfo64) 0.3.0 (SHA-256
`a24d2455af4efced6b261a2ed49da040a1e472c30899c68e7db377871db695bd`), part of
ICU4X. The data is Unicode data, licensed under Unicode-3.0; see
`zoneinfo64.LICENSE`. From the v8x source checkout, regenerate it with
`node tools/compress-quickjs-data.mjs <path-to-zoneinfo64.res>`.
