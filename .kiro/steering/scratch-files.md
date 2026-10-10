# Scratch files: never use /tmp

Do not read or write `/tmp`, `/private/tmp`, or `$TMPDIR` outside the workspace. Each access triggers a permission prompt for the user.

- Put all scratch files, logs, captures, and intermediate build output in `/Users/dexter/AVB-Audio/.agents/tmp/`. It is gitignored.
- Prefix tool commands that create temp files with `TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/`. This covers cargo, rustc, xcodebuild, clang, codesign, and mktemp.
- Pass `xcodebuild` an explicit `-derivedDataPath /Users/dexter/AVB-Audio/.agents/tmp/DerivedData`.
- Use `mktemp -p /Users/dexter/AVB-Audio/.agents/tmp`, never a bare `mktemp`.

# Never `cd` in shell commands

Every `cd` triggers a change-directory permission prompt, including `cd` into the scratch dir.

- Do not put `cd` (or `pushd`) in any command.
- Run every command from the workspace root and pass absolute paths.
- Use each tool's own directory flags instead:
  - `cargo --manifest-path /Users/dexter/AVB-Audio/Cargo.toml`
  - `xcodebuild -project /Users/dexter/AVB-Audio/driver/<name>.xcodeproj`
  - `git -C /Users/dexter/AVB-Audio`
  - `make -C <dir>`
- If a tool truly requires a working directory, use the shell tool's `cwd` parameter set to `/Users/dexter/AVB-Audio`, not a subdirectory.
