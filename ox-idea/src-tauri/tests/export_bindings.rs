//! `cargo test -p openidea --test export_bindings` regenerates `src/bindings.ts`
//! without launching the app (used by `pnpm gen:bindings` and CI drift checks).

#[test]
fn export_bindings() {
    openidea_lib::export_bindings(&openidea_lib::specta_builder());
}
