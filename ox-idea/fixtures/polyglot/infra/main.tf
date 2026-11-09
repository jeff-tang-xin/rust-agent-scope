terraform {
  required_version = ">= 1.6"
}

variable "environment" {
  type    = string
  default = "staging"
}

locals {
  name = "acme-${var.environment}"
}

output "service_name" {
  value = local.name
}
