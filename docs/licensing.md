# Licensing

Coda is licensed under the Apache License, Version 2.0. The full text is in `LICENSE`, and `NOTICE` states the license. The workspace `Cargo.toml` sets `license = "Apache-2.0"`, and every crate inherits it with `license.workspace = true`.

## Source headers

Every tracked `.rs`, `.sh`, `.yml` and `.yaml` file carries this line in its first three lines, inside a comment:

```
SPDX-License-Identifier: Apache-2.0
```

Use the comment style of the file: `//` in Rust, `#` in shell and YAML, `--` in SQL. In a shell script the line goes after the shebang. SQL files carry the line by convention, but the check does not cover them.

`scripts/check-spdx.sh` lists every file that lacks the line and exits 1. CI runs it. Run it locally before you push:

```sh
scripts/check-spdx.sh
```

A file that still carries a previous license identifier fails the check as well. `scripts/tests/check-spdx.test.sh` covers both cases.

## Dependencies

`cargo deny check licenses` enforces the allow list in `deny.toml`. It allows permissive licenses (MIT, Apache-2.0, Apache-2.0 WITH LLVM-exception, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, CDLA-Permissive-2.0) and the weak copyleft MPL-2.0. A dependency under any other license fails the check. Add a license to the list only after you confirm it is compatible with Apache-2.0.

## Contributions

A contribution is accepted under Apache-2.0, as section 5 of the license says. Add no code you do not have the right to license that way.
