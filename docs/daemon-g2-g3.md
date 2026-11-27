# Daemon G2 (isolate) + G3 (secure)

Branch: `feat/daemon-g2-g3` (dari `feat/daemon-g1-g4`, commit `1c335f4`).
Protocol: **1.2** (naik dari 1.1; method atau field baru wajib
menaikkan versi protokol, bukan cuma versi aplikasi).

Dokumen ini pasangan `docs/daemon-g1-g4.md`: state store, supervisor,
dan pola dispatch yang sama dipakai di sini, tidak diulang.

## G2: per-project runtime pin

### Method baru

| Method | Params | Hasil |
|---|---|---|
| `runtimes.list` | — | `{"runtimes": [{"name", "display", "binary", "version"}]}` |
| `runtimes.install` | `runtime`, `version?` | `{"runtime", "version", "installed": true}` |
| `projects.set_runtime` | `path`, `runtime`, `version?` | `{"path", "runtime", "version"}` |
| `projects.clear_runtime` | `path` | `{"path", "pinned": false}` |

`projects.list` (G1) diperkaya: tiap proyek mendapat field
`"runtime"`, `null` atau `{"runtime": "node", "version": "24.22.1"}`.

### Deteksi runtime (`runtimes.list`)

Daemon menjalankan perintah versi dari tiap binary yang dikenal dan
mengambil token semver pertama dari outputnya. Hanya runtime yang
benar-benar terpasang yang dilaporkan; binary yang hilang atau
outputnya tidak bisa diparse dilewati diam-diam.

| runtime | binary | perintah | contoh hasil |
|---|---|---|---|
| node | `node` | `--version` | `24.22.1` |
| npm | `npm` | `--version` | `10.9.4` |
| python | `python3` | `--version` | `3.12.3` |
| rust | `rustc` | `--version` | `1.100.0` |
| cargo | `cargo` | `--version` | `1.100.0` |
| go | `go` | `version` | `1.22.5` |
| php | `php` | `--version` | `8.3.0` |
| ruby | `ruby` | `--version` | `3.3.0` |
| composer | `composer` | `--version` | `2.7.1` |

Deteksi hanya menjalankan flag versi, tidak pernah mengeksekusi kode
proyek. Tiap perintah dibatasi 5 detik; yang menggantung dianggap
tidak terpasang. Probe yang melewati batas dimatikan dan di-reap
(dibunuh lalu ditunggu), jadi binary yang menggantung tidak
membocorkan thread dan proses anak tiap `runtimes.list`.
Deteksi jalan di `spawn_blocking` supaya tidak
memblokir loop async daemon.

### Pin (`projects.set_runtime`)

- `path` harus direktori proyek yang dikenal daemon (hasil scan dari
  parked path, atau linked). Path lain ditolak `-32602`.
- `runtime` harus salah satu nama di tabel di atas, kalau tidak
  `-32602` dengan daftar nama yang dikenal.
- `version` bila diisi harus sama persis dengan versi yang terdeteksi
  terpasang; versi lain atau runtime yang belum terpasang ditolak
  `-32602` dengan pesan yang menyebut versi yang terpasang.
- Pin disimpan di `state.json` (`runtime_pins`, satu pin per path;
  pin ulang menimpa pin lama) dan tampil di `projects.list`.
- `projects.clear_runtime` menghapus pin; path tanpa pin ditolak
  `-32602`. Clear tetap jalan walau direktorinya sudah dihapus
  (path diselesaikan secara leksikal, bukan canonicalize);
  `set_runtime` tetap menolak path yang tidak ada.
- Semua mutasi pin dan domain memakai `StateStore::apply`: bila
  `save()` gagal, state di memori dikembalikan, jadi pin atau record
  yang gagal tersimpan tidak muncul di `projects.list` / `domains.list`.

### Install (`runtimes.install`): cakupan yang jujur

Daemon ini **bukan** version manager dan tidak membangun satu.
Satu-satunya install yang dilakukan adalah mendelegasikan Rust ke
`rustup` bila manager itu sudah terpasang:

- `runtimes.install {"runtime": "rust", "version": "1.85.0"}`
  menjalankan `rustup toolchain install 1.85.0`. Tanpa `version`,
  toolchain-nya `stable`. Nama toolchain yang diterima: `stable`,
  `nightly`, `beta`, atau versi `X.Y.Z`.
- Versi yang dilaporkan adalah versi yang diminta bila ada; tanpa
  `version`, daemon menanyakan compiler toolchain itu secara
  eksplisit (`rustup run <toolchain> rustc --version`), bukan
  compiler aktif daemon, supaya `RUSTUP_TOOLCHAIN` tidak membuat
  laporan salah. Bila query itu gagal, nama toolchain yang
  dilaporkan.
- Bila `rustup` tidak ada: gagal `-32010` dengan pesan yang meminta
  install rustup dulu dari https://rustup.rs.
- Runtime lain (node, python, go, php, ruby, composer, npm, cargo):
  selalu gagal `-32010` dengan pesan install manual yang spesifik,
  mis. node diarahkan ke nodejs.org atau nvm, python ke package
  manager OS atau pyenv, go ke go.dev/dl.
- Runtime yang tidak dikenal: `-32010` dengan daftar nama yang
  dikenal.

Alasan: mengunduh dan mengelola toolchain dari nol adalah pekerjaan
version manager (rustup, nvm, pyenv, rbenv). Menduplikasinya di
daemon berarti menduplikasi bug keamanan dan logika platformnya.

## G3: local HTTPS

### Method baru

| Method | Params | Hasil |
|---|---|---|
| `domains.secure` | `domain` | `{"domain", "secured": true, "expires_at", "issuer": "Herd Local CA"}` |
| `domains.unsecure` | `domain` | `{"domain", "secured": false}` |
| `domains.list` | — | `{"domains": [{"domain", "project", "secured", "expires_at", "issuer"}]}` |

### Cara kerja

Satu local CA dibuat sekali per config dir, lalu satu sertifikat
leaf per domain yang di-secure. Semua lewat CLI `openssl`:

- CA: `openssl req -x509 -newkey rsa:2048 -days 3650 -subj
  "/CN=Herd Local CA"` dengan `basicConstraints=critical,CA:TRUE`
  dan `keyUsage=critical,keyCertSign,cRLSign`.
- Leaf: `openssl req -newkey rsa:2048 -subj "/CN=<domain>"` lalu
  `openssl x509 -req -CA ca.crt -CAkey ca.key -days 825` dengan
  `subjectAltName=DNS:<domain>` lewat file ekstensi sementara.
  Catatan: `-addext` pada `x509 -req` tidak ada di OpenSSL 3.0
  (baru di 3.2), jadi SAN ditulis lewat `-extfile`, bukan `-addext`.
- Pasangan pengganti saat re-secure dibuat di file temporer unik
  lalu dipasang dengan rename; key hidup tidak pernah ditulis
  langsung, jadi bila signing gagal pasangan lama tetap utuh.
  CN dibatasi 64 karakter (batas OpenSSL 3.0): domain yang lebih
  panjang dipotong (61 karakter + `...`), domain lengkap tetap di
  SAN. Pembuatan sertifikat diserialkan per config dir dengan
  mutex, jadi dua `domains.secure` bersamaan tidak bisa menimpa
  key/CSR satu sama lain.
- `expires_at` adalah unix timestamp = waktu pembuatan + 825 hari.
- File di `<config>/certs/`: `ca.key`, `ca.crt`,
  `<domain>.key`, `<domain>.crt`. Private key mode 0600. File
  `.csr`, `.ext`, dan `.srl` sementara dihapus sesudah dipakai,
  begitu juga file temporer key/crt dan backup saat re-secure.
- `state.json` menyimpan indeksnya (`domains`: domain, nama file
  cert/key, `created_at`, `expires_at`, `issuer`). File sertifikat
  yang hilang membuat `domains.list` melaporkan domain itu
  `secured: false` walau record-nya masih ada.

### Aturan

- Domain harus sudah dikenal daemon (linked lewat G1, atau
  auto-domain dari parked path). Domain yang belum terdaftar ditolak
  `-32602` ("link a project first"). Domain malformed ditolak
  `-32602` oleh normalisasi yang sama dengan G1.
- Bila `openssl` tidak ada di PATH: gagal `-32010` dengan pesan yang
  jelas, bukan diam-diam tanpa sertifikat.
- `domains.secure` yang diulang membuat ulang sertifikatnya (record
  di-upsert, masa berlaku dihitung ulang dari sekarang).
- `domains.unsecure` menghapus file cert/key dulu, baru record-nya.
  Bila penghapusan file gagal, handler mengembalikan error dan
  record dipertahankan, jadi operasi bisa diulang; file yang sudah
  hilang ditolerir, jadi retry setelah penghapusan parsial tetap
  jalan. Domain yang belum secured ditolak `-32602`.

### Yang SENGAJA tidak dibuat

- **Menyajikan sertifikat**: tidak ada nginx/Caddy wiring, tidak ada
  reverse proxy yang memakai cert ini. Sertifikat dibuat dan
  dicatat; server yang menyajikannya adalah pekerjaan berikutnya.
- **Trust ke system store**: CA tidak otomatis dipercaya browser/OS.
  Pengguna yang mau tanpa warning harus menambahkan `ca.crt` ke
  trust store-nya sendiri (atau memakai `mkcert` di luar daemon).
- **Renewal otomatis**: sertifikat berlaku 825 hari; tidak ada
  perpanjangan otomatis. Secure ulang bila mendekati kedaluwarsa.

## State

Dua field baru di `state.json`, keduanya `#[serde(default)]`,
sehingga file state v1.1 tetap bisa dibaca tanpa migrasi:

- `runtime_pins`: `[{path, runtime, version}]`
- `domains`: `[{domain, cert_file, key_file, created_at, expires_at, issuer}]`

Versi file state tetap `1` (penambahan field yang default-nya
kosong tidak butuh bump). Nama field dipilih supaya migrasi ke
SQLite nanti tinggal baca file sekali dan insert, seperti G1.

## CLI

```
herd runtimes list
herd runtimes install <runtime> [--version <version>]
herd projects set-runtime <path> <runtime> [--version <version>]
herd projects clear-runtime <path>
herd domains list
herd domains secure <domain>
herd domains unsecure <domain>
```

## Verifikasi

- `cargo test --workspace`: 32 lolos (26 lib herd-daemon, 1 g1_g4,
  4 g2_g3, 1 CLI). Baru di ronde perbaikan ini: 8 unit test
  (rollback `apply`, `stored_dir`, batas CN, re-secure cocok,
  domain panjang, pelaporan versi install, query toolchain,
  probe menggantung di-kill) dan 3 integration test (clear pin
  setelah direktori dihapus, save gagal tidak meninggalkan pin,
  unsecure gagal mempertahankan record).
- `cargo clippy --workspace --all-targets`: bersih kecuali 2 warning manifest
  bawaan v0.1.0 (`repository` dan `authors` di `workspace.package`).
- `cargo fmt --check`: bersih.
- Integration test `tests/g2_g3.rs` menjalankan daemon asli di
  port 0: `runtimes.list` (node + python, bentuk versi saja yang
  diasersi, bukan rilis tertentu, karena VPS menjalankan Node 22
  sementara sandbox Node 24), pin tampil di
  `projects.list`, penolakan versi/runtime yang salah, install
  node/zig gagal `-32010`, secure/unsecure dengan openssl asli
  (SAN dicek, `openssl verify` OK, key 0600, modulus key dan crt
  sama setelah secure ulang), dan bila openssl
  tidak ada, secure gagal `-32010`.

## Yang belum dites

- `runtimes.install` untuk Rust via rustup sungguhan (tidak ada
  toolchain yang diunduh di sandbox; yang dites hanya logika
  keputusannya dan penolakan runtime lain).
- Deteksi go/php/ruby/composer di mesin yang benar-benar punya
  binary-nya (sandbox hanya punya node, npm, python3, rust).
- Secure tanpa openssl (jalur error-nya ada di integration test
  tapi tidak tereksekusi di sandbox karena openssl ada).
- macOS/Windows (process spawn dan mode file 0600 baru
  terverifikasi di Linux).
