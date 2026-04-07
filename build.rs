fn main() {
    let git2 = vergen_git2::Git2Builder::default()
        .sha(true)
        .dirty(true)
        .commit_date(true)
        .build()
        .unwrap();
    vergen_git2::Emitter::default()
        .add_instructions(&git2)
        .unwrap()
        .emit()
        .unwrap();
}
