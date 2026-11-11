variable "project_id" { type = string }
variable "project_number" { type = string }
variable "billing_account" {
  type        = string
  description = "Billing account id (XXXXXX-XXXXXX-XXXXXX) for the budget"
}
variable "region" {
  type    = string
  default = "europe-west3" # Frankfurt; compare with asia-southeast1 via scripts/region-ab.sh
}
variable "zone" {
  type    = string
  default = "europe-west3-a"
}
variable "machine_type" {
  type        = string
  default     = "c4-standard-4"
  description = "4 vCPU high-clock. Downgrade to e2-standard-4 if benchmarks show no difference."
}
variable "disk_gb" {
  type    = number
  default = 50
}
variable "budget_usd" {
  type        = number
  default     = 1200
  description = "Planned 30-day GCP consumption (credits), NOT a hard cap"
}
variable "log_retention_days" {
  type    = number
  default = 7
}
