# Security policy

## Reporting a vulnerability

Report suspected vulnerabilities privately through GitHub private vulnerability reporting (Security tab, "Report a
vulnerability"), not in public issues or pull requests. Include the affected version, a
description, and steps to reproduce. You will get an answer within five working days.

## Principles

- The control plane is separate from the mock traffic and always requires authentication.
- Defaults are restrictive; anything that widens access is an explicit setting.
- Mock definitions are data, never code: no scripting engines, no file or environment access from templates.
- Outgoing traffic follows one egress policy.
- Secrets are masked wherever requests are recorded.
- Every change through the control plane is audited.
- Dependencies are kept current and checked against published advisories.
