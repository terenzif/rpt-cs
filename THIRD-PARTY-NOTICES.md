# Third-party notices

rpt-cs is a thin boundary over the [rpt-rs](https://github.com/MrSrsen/rpt-rs) engine
(pinned to revision `3f5beb51dd34d36c4a4326280d25f473b44889d1`), built as a `cdylib`
and consumed from .NET via P/Invoke.

## rpt-rs

MPL-2.0. The full license text ships in [`LICENSE`](LICENSE). The engine's own third-party
notices (krilla and its PDF/font dependencies, cosmic-text and its text-shaping stack, the
bundled Liberation and DejaVu font families, and the rest of its dependency tree) are
reproduced verbatim upstream in
[`THIRD-PARTY-NOTICES.md`](https://github.com/MrSrsen/rpt-rs/blob/main/THIRD-PARTY-NOTICES.md)
and are inherited by every binary built from this repository.

## Bundled fonts

The native library embeds the Liberation font family (SIL Open Font License 1.1) and the
DejaVu font family (Bitstream Vera licence), exactly as rpt-rs ships them in
`crates/rpt-text/fonts/`. Their licence texts ship with the upstream sources linked above.

## Trademarks

SAP and SAP Crystal Reports are trademarks or registered trademarks of SAP SE. rpt-cs is an
independent implementation reading the `.rpt` file format; it is not associated with,
endorsed by, or sponsored by SAP SE.