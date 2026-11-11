output "vm_ip" { value = google_compute_address.ip.address }
output "vm_service_account" { value = google_service_account.vm.email }
output "kek_key" { value = google_kms_crypto_key.kek.id }
output "secrets" { value = [for s in google_secret_manager_secret.s : s.secret_id] }
