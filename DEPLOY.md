# Release checklist

1. **Set one workspace version.** Update `[workspace.package].version` and every
   internal Solti dependency requirement in [Cargo.toml](Cargo.toml) to the same
   exact version. Refresh [Cargo.lock](Cargo.lock), then confirm every publishable
   package reports that version.

2. **Update versioned documentation.** When the major or minor release line
   changes, update `compatibility_line` in [docs/site.yml](docs/site.yml) and the
   public `solti` install snippets checked by `task ci/docs`. Keep patch releases
   on the existing compatibility line.

3. **Run the repository gates.** From the SDK root:

   ```bash
   task ci/fmt
   task ci/check
   task ci/clippy
   task ci/test/unit
   task ci/test/integration
   task ci/docs
   task ci/audit
   task ci/bench-check
   task ci/package
   ```

   Environment-gated containerd, cgroup, seccomp, and capability tests require
   their explicitly provisioned Linux hosts. A skipped lane is not a successful
   certification of that integration.

   Run `task ci/publish/dry-run` separately as a coordinated workspace archive
   check. Cargo stages the publishable workspace archives in a temporary
   registry and verifies them together without uploading them. The shared Rust
   workflow marks this check `continue-on-error`, so it is not a release gate.

4. **Check the crate order.** [`.github/crates.txt`](.github/crates.txt) must list
   every publishable workspace crate once, in dependency order, with `solti`
   last. The tag workflow verifies the list and requires every listed package
   version to equal the tag version.

5. **Use the matching validation path.** For a pull request, wait for the PR
   action's Rust and documentation jobs before merging. The `main` action detects
   the resulting merged-PR commit and skips duplicate validation. For a direct
   push to `main`, wait for the `main` action's Rust and documentation jobs on
   that commit. If pull-request origin cannot be resolved, the `main` action runs
   validation.

6. **Push the release tag.** Create and push `vX.Y.Z` for the intended commit
   after that commit is on `main`. The tag workflow verifies that the tag commit
   is an ancestor of `main`, publishes the listed crates, and then creates the
   non-draft, non-prerelease GitHub Release named `vX.Y.Z` with generated release
   notes and crate and documentation links.

7. **Check every publication.** Wait for
   [Tag publish](https://github.com/soltiHQ/sdk/actions/workflows/tag-publish.yml)
   to succeed. Verify every crate listed in `.github/crates.txt` on crates.io and
   docs.rs, including [solti](https://crates.io/crates/solti) and
   [its API documentation](https://docs.rs/solti).

The release workflow publishes crates sequentially. crates.io publication is
not transactional: a failure after an earlier crate succeeds can leave a partial
release. Before retrying, identify the crates already published under the version
and do not change their contents.

The SDK tag workflow publishes the crates and creates the GitHub Release. It does
not dispatch a documentation-site release; the site must first register the SDK
as an accepted product source.
