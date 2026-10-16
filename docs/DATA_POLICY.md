# Company Brain data policy

## Included data

The checked-in Company Brain example is entirely synthetic. Acme Cloud, Identity Engineering, Priya Raman, the Identity Platform, incident `INC-1042`, decision `DEC-014`, and both source records were created only for this demonstration. They do not describe real people, accounts, messages, incidents, or company records.

The canonical portable copy is [`demo/company-brain.seed.cypher`](../demo/company-brain.seed.cypher). The desktop's **Prepare demo company** control creates the same logical dataset with idempotent `MERGE` operations.

## Allowed inputs

Only ingest data that is one of the following:

- synthetic data created for the project;
- data owned by the participant or team and cleared for this use; or
- public data whose license permits the intended use and redistribution.

Do not ingest private, login-protected, paywalled, confidential, personal, or otherwise restricted material. Do not put access tokens, session cookies, passwords, private keys, or secret URLs in graph properties, fixtures, screenshots, logs, commits, or the demo video.

For every additional dataset, record its name, source URL, license, retrieval date, and any transformation in this document before submission.

## Tenant isolation and deletion

All Company Brain records are scoped by `tenant_id`. Retrieval and memory operations include that tenant in their match conditions. The supplied reset query deletes only records where `tenant_id = 'demo'`; it must not be changed to an unscoped `MATCH (n) DETACH DELETE n` on a shared database.

Memory history is append-only during normal operation. Retention and tenant erasure are separate, explicitly authorized operations in `core/src/company_memory.rs`.

## Credentials and generated data

Local `.env` files, Aqua token files, databases, audit logs, build output, and FalkorDB persistence are ignored by the root `.gitignore`. The environment template contains placeholders only. Production secrets belong in the deployment secret store or operating-system Credential Manager, never in source control.

Before publishing, review the complete tracked-file list and scan the entire history. If a real credential was ever published, revoke and rotate it; deleting it from a later commit is not sufficient.

## Participant attestation

Before submitting, the participant or team must confirm:

- all demo data is synthetic or properly licensed;
- no restricted data was loaded into the graph;
- every public asset has an appropriate attribution; and
- screenshots and video contain no credentials or personal information.

