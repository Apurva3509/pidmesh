# Security policy

Report vulnerabilities privately through GitHub's security advisory form for this repository. Do not
include secrets or private session content in a public issue.

PidMesh stores agent messages and memories unencrypted on the local filesystem. The database and its
default directory use owner-only permissions, but users remain responsible for device security and
for choosing a safe custom `PIDMESH_DB` path.
