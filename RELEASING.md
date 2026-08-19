# Releasing Harness Hat

The GitHub **Release** workflow does not run for ordinary pushes to `main`. It
starts only when a tag matching `v*` is pushed.

## Manual release

1. Merge the intended changes and check out the current `main` branch.
2. Confirm CI passes and the package version/changelog are ready.
3. Create and push the release tag:

   ```sh
   git tag v0.8.8
   git push origin v0.8.8
   ```

4. Follow the Release workflow and resulting GitHub release:

   ```sh
   gh run list --workflow Release --limit 5
   gh release view v0.8.8
   ```

Replace `v0.8.8` with the intended version. Do not tag a pull-request branch or
create a release merely to test the workflow.

A manually pushed tag needs no custom secret. The release workflow declares
`contents: write` and uses GitHub's built-in token.

## Tags created by another workflow

GitHub suppresses downstream workflow triggers for tags pushed with the
tag-producing workflow's default `GITHUB_TOKEN`. To automate tag creation:

1. Create a fine-grained personal access token at **GitHub Settings → Developer
   settings → Personal access tokens → Fine-grained tokens**.
2. Restrict it to `only-cliches/harness-hat` with **Contents: Read and write**.
3. Add it at **Repository Settings → Secrets and variables → Actions → New
   repository secret** as `RELEASE_TOKEN`.
4. Configure the tag-producing workflow's checkout/push credentials to use
   `secrets.RELEASE_TOKEN`, then push the `v*` tag.

The Release workflow itself should continue using its built-in token. macOS
notarization and platform code-signing credentials are separate and are not
currently configured.

## Expected archives

- Linux ZIP: `hat`, `hat-daemon`
- macOS ZIP: `hat`, `hat-daemon`, `hat-launcher`, and a self-contained
  `Harness Hat.app` whose `Contents/MacOS` also carries all three executables
- Windows ZIP: `hat.exe`, `hat-daemon.exe`, `hat-launcher.exe`
