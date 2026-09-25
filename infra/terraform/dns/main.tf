terraform {
  required_version = ">= 1.11, < 2.0"
  required_providers {
    cloudflare = {
      source  = "cloudflare/cloudflare"
      version = "~> 5.25.0"
    }
  }
}

# Read CLOUDFLARE_API_TOKEN from the operator environment, not tfvars/state.
provider "cloudflare" {}

variable "zone_id" {
  type        = string
  description = "Existing Cloudflare zone ID; registrar/NS changes are a separate cutover."
  validation {
    condition     = can(regex("^[0-9a-f]{32}$", var.zone_id))
    error_message = "Supply a Cloudflare zone ID."
  }
}
variable "domain" {
  type    = string
  default = "hibana.cloud"
}
variable "ingress_ipv4" {
  type        = string
  description = "Public IPv4 of the management node."
  validation {
    condition     = can(cidrnetmask("${var.ingress_ipv4}/32"))
    error_message = "Supply an IPv4 address."
  }
}
variable "tenant_slugs" {
  type    = set(string)
  default = ["local"]
  validation {
    condition     = length(var.tenant_slugs) > 0 && alltrue([for name in var.tenant_slugs : can(regex("^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$", name))])
    error_message = "Tenant slugs must be valid DNS labels."
  }
}

variable "disable_domain_mail" {
  type        = bool
  default     = false
  description = "Preserve a no-mail domain's null MX and deny-all SPF. Do not enable for a domain used for email."
}

locals {
  names = setunion(toset([var.domain, "auth.${var.domain}"]), toset([for tenant in var.tenant_slugs : "*.${tenant}.apps.${var.domain}"]))
}

resource "cloudflare_dns_record" "hibana" {
  for_each = local.names
  zone_id  = var.zone_id
  name     = each.key
  type     = "A"
  content  = var.ingress_ipv4
  ttl      = 300
  # TLS terminates at Traefik. Cloudflare Universal SSL does not cover these
  # nested tenant names; DNS-only also preserves streaming and upload behavior.
  proxied = false
  comment = "Managed by Hibana VPS Terraform"
}

resource "cloudflare_dns_record" "null_mx" {
  count    = var.disable_domain_mail ? 1 : 0
  zone_id  = var.zone_id
  name     = var.domain
  type     = "MX"
  content  = "."
  priority = 0
  ttl      = 300
  comment  = "Domain does not accept email; managed by Hibana VPS Terraform"
}

resource "cloudflare_dns_record" "deny_mail_spf" {
  count   = var.disable_domain_mail ? 1 : 0
  zone_id = var.zone_id
  name    = var.domain
  type    = "TXT"
  content = "v=spf1 -all"
  ttl     = 300
  comment = "Domain does not send email; managed by Hibana VPS Terraform"
}

output "records" {
  value = { for name, record in cloudflare_dns_record.hibana : name => record.content }
}
