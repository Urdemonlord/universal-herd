# Daemon G1 (park/link) dan G4 (services)

Branch: `feat/daemon-g1-g4`. Protocol version naik 1.0 -> 1.1, karena
method baru ditambahkan (BSD: perubahan method menaikkan versi
protokol, bukan hanya versi aplikasi). Method lama `system.status`
dan `projects.scan` tidak berubah perilakunya, kecuali field
`protocol` di `system.status` yang kini `"1.1"`.

## State file

Lokasi: OS config dir yang sama dengan `daemon.token`
(`state.json`, mode 0600 di Unix). Contoh isi:

```json
{
  "version": 1,
  "parked_paths": ["/home/arya/code"],
  "links": [{ "path": "/home/arya/client-portal", "domain": "client-portal.test" }],
  "services": [
    { "name": "redis", "command": "redis-server", "args": [], "port": 6379, "workdir": null, "kind": "redis" }
  ]
}
```

Penulisan atomik: tulis ke `state.json.tmp.<pid>`, fsync, lalu rename.
File korup atau versi tidak dikenal membuat daemon gagal start dengan
pesan jelas, tidak pernah me-reset state secara diam-diam.

Log service tinggal di `<config>/logs/<name>.log` (append, stdout dan
stderr digabung). PID dan status running hanya ada di memori: daemon
restart tidak menghidupkan service lagi; service harus di-start ulang
secara eksplisit. Belum ada auto-start (IPD D-7).

### Jalur migrasi ke SQLite

SQLite tetap rencana (BSD §7, IPD D-1), bukan cakupan kerja ini.
Nama field di atas dipilih agar sama dengan kolom tabel BSD
(`parked_paths`, `links`, `services`), jadi migrasi nanti berarti:
baca `state.json` sekali, insert tiap baris ke skema SQLite, lalu
tulis `state.json.migrated` sebagai penanda dan berhenti membacanya.
Tidak ada data yang hilang dalam proses itu: token, path, dan definisi
service semuanya punya kolom padanan di BSD §7.

## Method baru

Semua method memakai autentikasi token yang sama seperti method lama.
Error validasi input: `-32602`. Error internal (state gagal disimpan,
binary tidak ditemukan, log tidak bisa dibaca): `-32010`. Method tidak
dikenal: `-32601`.

### G1: park/link

| Method | Params | Hasil |
|---|---|---|
| `projects.park` | `path` (direktori, harus ada) | `{"path", "parked": true}` |
| `projects.unpark` | `path` | `{"path", "parked": false}` |
| `projects.link` | `path`, `domain` | `{"path", "domain"}` |
| `projects.unlink` | `path` | `{"path", "linked": false}` |
| `projects.list` | `max_depth?` (default 3) | `{"projects": [...]}` |

Aturan domain (`normalize_domain`): huruf kecil, suffix default
`.test` bila tidak ada titik (`myapp` -> `myapp.test`), label hanya
`[a-z0-9-]`, tidak boleh kosong, tidak boleh diawali/diakhiri `-`,
tidak boleh ada label kosong. Domain duplikat dan path yang sudah
ter-link ditolak dengan `-32602`. Path di-canonicalize sebelum
disimpan, jadi `/home/arya/code/../code` dan `/home/arya/code`
dianggap path yang sama. `unpark`/`unlink` tetap bisa menghapus
registrasi yang direktorinya sudah dihapus: path dicocokkan dengan
path canonical yang tersimpan tanpa mengharuskan direktori masih ada.

`projects.list` memindai tiap parked path (kedalaman 3) dan menambahkan
tiap linked path secara langsung. Tiap proyek diperkaya:

- `source: "Linked"` bila path-nya sama persis dengan link, domain
  dari link itu.
- `source: "Parked"` bila proyek berada langsung di dalam (atau sama
  dengan) parked path, domain otomatis dari nama folder
  (`my_app` -> `my-app.test`).
- `source: "Scanned"` bila ditemukan lebih dalam di bawah parked path,
  tanpa domain.

Contoh:

```json
{"jsonrpc": "2.0", "id": 1, "method": "projects.list", "params": {}, "token": "<token>"}
```

```json
{"jsonrpc": "2.0", "id": 1, "result": {"projects": [
  {"name": "web", "path": "/home/arya/code/web", "kind": "Node.js", "manifest": "package.json", "domain": "web.test", "source": "Parked"}
]}}
```

### G4: services

| Method | Params | Hasil |
|---|---|---|
| `services.list` | - | `{"services": [...]}` |
| `services.create` | `name`, `command?`, `args?`, `port?`, `workdir?` | definisi service |
| `services.start` | `name` | `{"name", "pid", "status": "running"}` |
| `services.stop` | `name` | `{"name", "status": "stopped", "exit_code"}` |
| `services.remove` | `name` | `{"name", "removed": true}` |
| `services.logs` | `name`, `lines?` (default 100, maks 1000) | `{"name", "lines": [...]}` |

Aturan nama service: 1-64 karakter, hanya `[a-z0-9-]`, tidak boleh
diawali/diakhiri `-`, harus unik. `workdir` bila diberikan harus
direktori yang ada. `port` 1-65535.

Katalog default: bila `command` tidak diberikan dan `name` dikenal,
binary dan port diisi otomatis: `postgres` -> `postgres:5432`,
`mysql` -> `mysqld:3306`, `redis` -> `redis-server:6379`,
`mailpit` -> `mailpit:1025`. Nama lain wajib menyertakan `command`.
Katalog hanya default definisi; `services.start` tetap memeriksa
binary ada di PATH dan gagal dengan pesan jelas bila tidak ada.

`services.list` tiap service: `status` (`running`, `stopped`,
`failed`), `pid`, `started_at` (epoch), `healthy`, `last_exit_code`.
Health: bila service mendeklarasikan port, TCP connect ke
`127.0.0.1:port` harus berhasil; bila tidak ada port, hidup berarti
sehat. `failed` berarti proses mati sendiri dengan exit code bukan 0
atau mati karena sinyal; service yang di-stop supervisor tercatat
`stopped`, bukan `failed`.

Supervisi: proses anak di-spawn sebagai process-group leader di Unix,
jadi `stop` mengirim SIGTERM ke seluruh group (anak + cucu), lalu
menunggu sampai group itu kosong (bukan hanya sampai anak langsung
keluar) selama maksimal 3 detik, lalu SIGKILL anggota yang tersisa.
Di Windows tidak ada process group: `stop` membunuh anak langsung
saja (`Child::kill`), jadi cucu yang di-spawn service (misalnya
background job dari shell) bisa selamat. Itu keterbatasan yang
diketahui, bukan bug yang disembunyikan: tes `stop_kills_direct_child_on_windows`
membuktikan anak langsung mati, tes group kill hanya jalan di Unix
(`#[cfg(unix)]`). Service yang keluar sendiri dalam 100 ms pertama setelah
start dilaporkan gagal start beserta exit code-nya, bukan dilaporkan
berhasil lalu mati diam-diam. `services.logs` membaca dari akhir file
dengan buffer terbatas (maksimal ~1 MiB), jadi log yang tumbuh sampai
gigabyte tidak membebani memori daemon; satu baris yang luar biasa
panjang dipotong di 64 KiB.

Contoh:

```json
{"jsonrpc": "2.0", "id": 2, "method": "services.create", "params": {"name": "redis"}, "token": "<token>"}
{"jsonrpc": "2.0", "id": 3, "method": "services.start", "params": {"name": "redis"}, "token": "<token>"}
{"jsonrpc": "2.0", "id": 4, "method": "services.logs", "params": {"name": "redis", "lines": 20}, "token": "<token>"}
```

## CLI

```console
herd projects park ~/code
herd projects link ~/client-portal client-portal
herd projects list
herd projects unpark ~/code
herd projects unlink ~/client-portal

herd services create redis
herd services create websvc --command python3 --arg -m --arg http.server --arg 8080 --port 8080
herd services list
herd services start redis
herd services logs redis --lines 50
herd services stop redis
herd services remove redis
```

## Yang sengaja tidak dibuat

- **SQLite**: rencana (IPD D-1), bukan tugas ini. Lihat jalur migrasi
  di atas.
- **Auto-start service saat daemon restart**: tidak ada. Service
  adalah proses yang diawasi, bukan unit OS (IPD D-7).
- **Restart policy**: belum ada. Service yang crash tercatat
  `failed` beserta exit code, tidak di-restart otomatis.
- **Port allocation/konflik**: belum ada. Dua service boleh
  mendeklarasikan port yang sama; health check yang akan menunjukkan
  service mana yang benar-benar menjawab.
- **G2 (isolate/runtime pin) dan G3 (secure/HTTPS)**: bukan cakupan
  tugas ini; lihat `LARAVEL-HERD-GAP.md` §4.
- **Daemon sebagai user service OS** (systemd/launchd): belum ada.

## Test dan override dev

`cargo test` menjalankan unit test (state store, validasi domain/nama
service, supervisor, serialisasi) dan satu integration test
(`crates/herd-daemon/tests/g1_g4.rs`) yang menjalankan dispatch
daemon asli di listener port 0 dengan state di temp dir.

`HERD_CONFIG_DIR` meng-override config dir (dipakai integration test
dan pengujian manual). Contoh pengujian manual:

```console
HERD_CONFIG_DIR=/tmp/herd-test herd daemon
```
