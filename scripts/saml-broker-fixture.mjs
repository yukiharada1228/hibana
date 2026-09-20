// Disposable Keycloak realms only. The corporate realm is a real SAML IdP;
// Hibana trusts only the existing broker realm's OIDC issuer.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

export async function createSamlBrokerFixture(idp) {
  const template = JSON.parse(await readFile(
    new URL("../deploy/keycloak/saml-identity-provider.example.json", import.meta.url), "utf8",
  ));
  const alias = template.alias;
  const upstream = `${idp}/realms/corporate`;
  const broker = `${idp}/realms/hibana`;
  const endpoint = `${broker}/broker/${alias}/endpoint`;
  const entityId = `${broker}/broker/${alias}`;
  const password = "fixture-corporate-password";
  const people = [
    { name: "employee", source: "10000000-0000-4000-8000-000000000001", subject: "20000000-0000-4000-8000-000000000001" },
    { name: "outsider", source: "10000000-0000-4000-8000-000000000002", subject: "20000000-0000-4000-8000-000000000002" },
  ];
  let credentials;
  let refreshAt = 0;
  async function admin(path, { method = "GET", body } = {}) {
    if (Date.now() >= refreshAt) {
      const response = await fetch(`${idp}/realms/master/protocol/openid-connect/token`, {
        method: "POST",
        body: new URLSearchParams({
          client_id: "admin-cli", grant_type: "password",
          username: "fixture-admin", password: "fixture-admin-password",
        }),
        signal: AbortSignal.timeout(10_000),
      });
      assert.equal(response.status, 200);
      credentials = await response.json();
      refreshAt = Date.now() + (credentials.expires_in - 10) * 1000;
    }
    const response = await fetch(`${idp}/admin${path}`, {
      method,
      headers: { authorization: `Bearer ${credentials.access_token}`, "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(10_000),
    });
    assert.ok(response.ok, `SAML fixture ${method} ${path}: ${response.status}`);
    const text = await response.text();
    return text ? JSON.parse(text) : undefined;
  }
  await admin("/realms", { method: "POST", body: {
    realm: "corporate", enabled: true, sslRequired: "none",
    duplicateEmailsAllowed: true,
    users: people.map((person) => ({
      id: person.source, username: person.name, enabled: true,
      email: "corporate-shared@example.invalid", emailVerified: true,
      firstName: person.name, lastName: "Fixture",
      credentials: [{ type: "password", value: password, temporary: false }],
    })),
  } });
  const certificate = async (realm) => {
    const keys = await admin(`/realms/${realm}/keys`);
    const key = keys.keys.find((key) => key.kid === keys.active.RS256);
    assert.ok(key?.certificate, "realm must publish its active signing certificate");
    return key.certificate;
  };
  const brokerCertificate = await certificate("hibana");
  const corporateCertificate = await certificate("corporate");
  await admin("/realms/corporate/clients", { method: "POST", body: {
    clientId: entityId, name: "Hibana SAML broker", protocol: "saml", enabled: true,
    redirectUris: [endpoint], fullScopeAllowed: false,
    attributes: {
      "saml_assertion_consumer_url_post": endpoint,
      "saml.force.post.binding": "true",
      "saml.server.signature": "true",
      "saml.assertion.signature": "true",
      "saml.client.signature": "true",
      "saml.signing.certificate": brokerCertificate,
      "saml.signature.algorithm": "RSA_SHA256",
      "saml.authnstatement": "true",
      "saml_name_id_format": "persistent",
    },
    protocolMappers: [{
      name: "stable-employee-id", protocol: "saml", protocolMapper: "saml-user-property-mapper",
      config: {
        "user.attribute": "id", "attribute.name": "employee_id",
        "attribute.nameformat": "Basic",
      },
    }],
  } });
  const provider = {
    ...template,
    config: {
      ...template.config,
      entityId, idpEntityId: upstream, singleSignOnServiceUrl: `${upstream}/protocol/saml`,
      signingCertificate: corporateCertificate,
    },
  };
  await admin("/realms/hibana/identity-provider/instances", { method: "POST", body: provider });
  for (const person of people) {
    // Explicit external-ID links avoid automatic account linking by email.
    // Broker users have no password; only the corporate realm can log them in.
    await admin("/realms/hibana/users", { method: "POST", body: {
      id: person.subject, username: `saml-${person.name}`, enabled: true,
      email: "corporate-shared@example.invalid", emailVerified: true,
      firstName: person.name, lastName: "Fixture",
      federatedIdentities: [{ identityProvider: alias, userId: person.source, userName: person.name }],
    } });
    const users = await admin(`/realms/hibana/users?username=saml-${person.name}&exact=true`);
    assert.equal(users.length, 1);
    person.subject = users[0].id;
    assert.deepEqual(await admin(`/realms/hibana/users/${person.subject}/credentials`), []);
    const links = await admin(`/realms/hibana/users/${person.subject}/federated-identity`);
    assert.ok(links.some((link) => link.identityProvider === alias && link.userId === person.source));
  }
  return { alias, upstream, broker, endpoint, entityId, password, people, admin, provider };
}
