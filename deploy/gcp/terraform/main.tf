# Minimal single-VM private beta. `terraform plan` is safe; `terraform apply`
# creates billable resources and REQUIRES explicit owner approval.
terraform {
  required_version = ">= 1.6"
  required_providers {
    google = { source = "hashicorp/google", version = "~> 6.0" }
  }
}

provider "google" {
  project = var.project_id
  region  = var.region
  zone    = var.zone
}

locals {
  apis = ["compute.googleapis.com", "secretmanager.googleapis.com", "cloudkms.googleapis.com", "billingbudgets.googleapis.com", "logging.googleapis.com", "monitoring.googleapis.com", "iap.googleapis.com"]
}

resource "google_project_service" "apis" {
  for_each           = toset(local.apis)
  service            = each.value
  disable_on_destroy = false
}

# --- identity: dedicated SA, no keys, least privilege
resource "google_service_account" "vm" {
  account_id   = "trader-vm"
  display_name = "trader VM runtime"
}

resource "google_project_iam_member" "log_writer" {
  project = var.project_id
  role    = "roles/logging.logWriter"
  member  = "serviceAccount:${google_service_account.vm.email}"
}

resource "google_project_iam_member" "metric_writer" {
  project = var.project_id
  role    = "roles/monitoring.metricWriter"
  member  = "serviceAccount:${google_service_account.vm.email}"
}

# --- secrets (values are added out-of-band with `gcloud secrets versions add`; never in TF state)
resource "google_secret_manager_secret" "s" {
  for_each  = toset(["telegram-bot-token", "telegram-webhook-secret", "kek-base64", "db-password", "admin-token", "rpc-url"])
  secret_id = "trader-${each.value}"
  replication {
    auto {}
  }
  depends_on = [google_project_service.apis]
}

resource "google_secret_manager_secret_iam_member" "access" {
  for_each  = google_secret_manager_secret.s
  secret_id = each.value.id
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.vm.email}"
}

# --- KMS key reserved for KEK wrapping (KMS-backed envelope encryption is a
# documented next step; the current build reads the KEK from Secret Manager).
resource "google_kms_key_ring" "kr" {
  name       = "trader"
  location   = var.region
  depends_on = [google_project_service.apis]
}

resource "google_kms_crypto_key" "kek" {
  name            = "wallet-kek"
  key_ring        = google_kms_key_ring.kr.id
  purpose         = "ENCRYPT_DECRYPT"
  rotation_period = "7776000s" # 90 days
  lifecycle { prevent_destroy = true }
}

resource "google_kms_crypto_key_iam_member" "kek_use" {
  crypto_key_id = google_kms_crypto_key.kek.id
  role          = "roles/cloudkms.cryptoKeyEncrypterDecrypter"
  member        = "serviceAccount:${google_service_account.vm.email}"
}

# --- network
resource "google_compute_address" "ip" {
  name       = "trader-ip"
  depends_on = [google_project_service.apis]
}

resource "google_compute_firewall" "https" {
  name          = "trader-allow-https"
  network       = "default"
  direction     = "INGRESS"
  source_ranges = ["0.0.0.0/0"] # Mini App users + Telegram webhooks (webhook path also requires the secret header)
  allow {
    protocol = "tcp"
    ports    = ["80", "443"]
  }
  target_tags = ["trader"]
}

resource "google_compute_firewall" "ssh_iap_only" {
  name          = "trader-allow-ssh-iap"
  network       = "default"
  direction     = "INGRESS"
  source_ranges = ["35.235.240.0/20"] # Google IAP TCP forwarding range only
  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
  target_tags = ["trader"]
}

# --- VM
resource "google_compute_instance" "vm" {
  name         = "trader-vm"
  machine_type = var.machine_type
  tags         = ["trader"]
  labels       = { app = "trader", env = "private-beta" }

  boot_disk {
    initialize_params {
      image = "debian-cloud/debian-12"
      size  = var.disk_gb
      type  = "pd-balanced"
    }
  }

  network_interface {
    network = "default"
    access_config { nat_ip = google_compute_address.ip.address }
  }

  service_account {
    email  = google_service_account.vm.email
    scopes = ["cloud-platform"]
  }

  shielded_instance_config {
    enable_secure_boot          = true
    enable_vtpm                 = true
    enable_integrity_monitoring = true
  }

  metadata = {
    enable-oslogin = "TRUE"
    startup-script = file("${path.module}/../vm/startup.sh")
  }

  scheduling {
    automatic_restart   = true
    on_host_maintenance = "MIGRATE"
  }

  depends_on = [google_project_service.apis]
}

# --- daily disk snapshots, 7-day retention
resource "google_compute_resource_policy" "snap" {
  name = "trader-daily-snapshots"
  snapshot_schedule_policy {
    schedule {
      daily_schedule {
        days_in_cycle = 1
        start_time    = "03:00"
      }
    }
    retention_policy {
      max_retention_days    = 7
      on_source_disk_delete = "KEEP_AUTO_SNAPSHOTS"
    }
  }
}

resource "google_compute_disk_resource_policy_attachment" "snap" {
  name = google_compute_resource_policy.snap.name
  disk = google_compute_instance.vm.name
}

# --- short log retention to bound logging cost
resource "google_logging_project_bucket_config" "default" {
  project        = var.project_id
  location       = "global"
  bucket_id      = "_Default"
  retention_days = var.log_retention_days
}

# --- budget alerts. NOTE: alerts do NOT cap spend. See docs/COST_AND_SHUTDOWN.md.
resource "google_billing_budget" "budget" {
  billing_account = var.billing_account
  display_name    = "trader-30-day-plan"
  budget_filter {
    projects               = ["projects/${var.project_number}"]
    credit_types_treatment = "EXCLUDE_ALL_CREDITS" # track gross usage = credit burn
  }
  amount {
    specified_amount {
      currency_code = "USD"
      units         = tostring(var.budget_usd)
    }
  }
  dynamic "threshold_rules" {
    for_each = [0.25, 0.5, 0.8, 0.95, 1.0]
    content {
      threshold_percent = threshold_rules.value
    }
  }
  threshold_rules {
    threshold_percent = 0.8
    spend_basis       = "FORECASTED_SPEND"
  }
}
