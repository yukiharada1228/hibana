mock_provider "cloudflare" {}
variables {
  zone_id      = "0123456789abcdef0123456789abcdef"
  ingress_ipv4 = "203.0.113.12"
  tenant_slugs = ["local", "team"]
}
run "tenant_dns_only" {
  command = plan
  assert {
    condition     = length(cloudflare_dns_record.hibana) == 4
    error_message = "Provision console, identity and one wildcard per tenant."
  }
  assert {
    condition     = alltrue([for r in cloudflare_dns_record.hibana : !r.proxied && r.content == var.ingress_ipv4])
    error_message = "All records must route directly to the ingress node."
  }
  assert {
    condition     = contains(keys(cloudflare_dns_record.hibana), "*.team.apps.hibana.cloud")
    error_message = "A single *.apps wildcard cannot cover app.tenant.apps names."
  }
}
run "reject_invalid_slug" {
  command = plan
  variables { tenant_slugs = ["bad.tenant"] }
  expect_failures = [var.tenant_slugs]
}

run "preserve_no_mail_policy" {
  command = plan
  variables { disable_domain_mail = true }
  assert {
    condition     = cloudflare_dns_record.null_mx[0].content == "." && cloudflare_dns_record.null_mx[0].priority == 0 && cloudflare_dns_record.deny_mail_spf[0].content == "v=spf1 -all"
    error_message = "Preserve the existing null MX and deny-all SPF during the registrar DNS cutover."
  }
}
