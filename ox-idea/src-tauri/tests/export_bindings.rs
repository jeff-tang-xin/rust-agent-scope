//! `cargo test -p oxidea --test export_bindings` regenerates `src/bindings.ts`
//! without launching the app (used by `pnpm gen:bindings` and CI drift checks).

#[test]
fn export_bindings() {
    oxidea_lib::export_bindings(&oxidea_lib::specta_builder());
}
