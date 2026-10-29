Before and after captures for the link preview pull request.

Made from that branch over the synthetic demo data:

    cargo run --features demo -- --demo-page link-preview,light --demo-shot after-light.png
    cargo run --features demo -- --demo-page link-preview,light --demo-shot after-dark.png
    cargo run --features demo -- --demo-page link-preview-off,light --demo-shot before-light.png
    cargo run --features demo -- --demo-page link-preview-off,light --demo-shot before-dark.png

They live on this branch so the pull request description can show them; they are
not meant to be merged anywhere.
