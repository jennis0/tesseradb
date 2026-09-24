# HTTP API

This page is rendered from [`tessera.yaml`](../openapi/tessera.yaml), the OpenAPI 3.1 description of the viewer and session planes. The control plane, which declares, ingests, deletes, suppresses and publishes layers, is not in the description yet; [Capabilities](capabilities.md) lists its routes. The bodies of `POST /v1/viewport` and `POST /v1/items` are framed Arrow streams, described in [Wire framing](../openapi/README.md).

[OAD(../openapi/tessera.yaml)]
