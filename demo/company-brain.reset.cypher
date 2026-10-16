// Delete only the synthetic demo tenant. Never replace this with an unscoped graph delete.
MATCH (node)
WHERE node.tenant_id = 'demo'
DETACH DELETE node;

