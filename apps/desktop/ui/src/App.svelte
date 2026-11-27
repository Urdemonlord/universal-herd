<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';
  import { LOGOS } from './logos';

  interface Project {
    name: string;
    path: string;
    kind: string;
    manifest: string;
    domain: string | null;
    source: string;
    runtime: { runtime: string; version: string } | null;
  }
  interface Runtime {
    name: string;
    display: string;
    binary: string;
    version: string;
  }
  interface Service {
    name: string;
    command: string;
    port: number | null;
    status: string;
    pid: number | null;
    healthy: boolean | null;
  }
  interface Domain {
    domain: string;
    project: string;
    secured: boolean;
    expires_at: string | null;
  }

  let active = $state('Overview');
  let settingsSection = $state('Daemon');
  let connected = $state(false);
  let daemonProtocol = $state('');
  let starting = $state(false);
  let loading = $state(true);
  let error = $state('');
  let notice = $state('');
  let busy = $state('');
  let projects = $state<Project[]>([]);
  let runtimes = $state<Runtime[]>([]);
  let services = $state<Service[]>([]);
  let domains = $state<Domain[]>([]);
  let scanPath = $state('');
  let parkPath = $state('');
  let linkPath = $state('');
  let linkDomain = $state('');
  let showProjectForms = $state(false);
  let projectFilter = $state('All');
  let projectSearch = $state('');
  let linkDomainInput: HTMLInputElement | undefined = $state();

  // Lucide-style line icons (24x24, stroke) matching the FigCraft frames.
  // Overview = layout-grid, Projects = folder, Runtimes = package,
  // Services = play, Domains = globe, Containers = layers,
  // Plugins = 2x2 tiles (mirrors the FigCraft empty-state illustration),
  // Settings = gear (circle + 8 spokes).
  const NAV_ICONS: Record<string, string> = {
    Overview:
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/></svg>',
    Projects:
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.2 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/></svg>',
    Runtimes:
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 21.73a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73z"/><path d="M12 22V12"/><path d="m3.3 7 7.703 4.734a2 2 0 0 0 1.994 0L20.7 7"/><path d="m7.5 4.27 9 5.15"/></svg>',
    Services:
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polygon points="6 3 20 12 6 21 6 3"/></svg>',
    Domains:
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"/><path d="M2 12h20"/></svg>',
    Containers:
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m12.83 2.18a2 2 0 0 0-1.66 0L2.6 6.08a1 1 0 0 0 0 1.83l8.58 3.91a2 2 0 0 0 1.66 0l8.58-3.9a1 1 0 0 0 0-1.83Z"/><path d="m22 17.65-9.17 4.16a2 2 0 0 1-1.66 0L2 17.65"/><path d="m22 12.65-9.17 4.16a2 2 0 0 1-1.66 0L2 12.65"/></svg>',
    Plugins:
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="8" height="8" rx="1" fill="currentColor" stroke="none"/><rect x="13" y="3" width="8" height="8" rx="1" fill="currentColor" stroke="none"/><rect x="3" y="13" width="8" height="8" rx="1" fill="currentColor" stroke="none"/><rect x="13" y="13" width="8" height="8" rx="1" stroke-dasharray="2.5 2"/></svg>',
    Settings:
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3.2"/><path d="M12 2.5v3M12 18.5v3M2.5 12h3M18.5 12h3M5 5l2.1 2.1M16.9 16.9 19 19M19 5l-2.1 2.1M7.1 16.9 5 19"/></svg>',
  };
  const ACTIVITY_ICON =
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 12h-4l-3 9L9 3l-3 9H2"/></svg>';

  const navigation = [
    'Overview',
    'Projects',
    'Runtimes',
    'Services',
    'Domains',
    'Containers',
    'Plugins',
    'Settings',
  ];

  const settingsNav = ['General', 'Daemon', 'Runtimes', 'Domains', 'Parked Paths'];

  const pageMeta: Record<string, { sub: string }> = {
    Overview: { sub: 'One environment. Infinite extensions.' },
    Projects: { sub: 'All projects on this machine.' },
    Runtimes: { sub: 'Languages and tools available on this machine.' },
    Services: { sub: 'Databases, caches, and background services.' },
    Domains: { sub: 'Local domains and reverse proxy.' },
    Containers: { sub: 'Docker and Podman containers on this machine.' },
    Plugins: { sub: 'Third-party extensions for Herd.' },
    Settings: { sub: 'App and daemon configuration.' },
  };

  function fail(e: unknown): string {
    return e instanceof Error ? e.message : String(e);
  }

  function runningServices(): number {
    return services.filter((s) => s.status === 'running').length;
  }

  function doneSteps(): number {
    let n = 0;
    if (runtimes.length > 0) n += 1;
    if (projects.length > 0) n += 1;
    if (runningServices() > 0) n += 1;
    return n;
  }

  function projectKinds(): string[] {
    return [...new Set(projects.map((p) => p.kind))];
  }

  // "6 Rust + 1 Node.js", the kind breakdown FigCraft shows in the Projects
  // count label and the Projects stat card subtitle.
  function projectKindSummary(): string {
    const counts = new Map<string, number>();
    for (const p of projects) counts.set(p.kind, (counts.get(p.kind) ?? 0) + 1);
    const s = [...counts.entries()].map(([k, n]) => `${n} ${k}`).join(' + ');
    return s.length > 26 ? `${projects.length} projects` : s;
  }

  // First three runtime names plus a "+N" overflow, kept short enough to fit
  // one line on a stat card ("Node.js, npm, Python +2").
  function runtimeNameSummary(): string {
    const names = runtimes.map((r) => r.display);
    let s = names.slice(0, 3).join(', ');
    if (names.length > 3) s += ` +${names.length - 3}`;
    if (s.length > 26) s = `${names.slice(0, 2).join(', ')} +${names.length - 2}`;
    return s;
  }

  function filteredProjects(): Project[] {
    const q = projectSearch.trim().toLowerCase();
    return projects.filter((p) => {
      if (projectFilter !== 'All' && p.kind !== projectFilter) return false;
      if (q && !p.name.toLowerCase().includes(q) && !p.path.toLowerCase().includes(q)) return false;
      return true;
    });
  }

  function runtimeSourceCounts(): { path: number; cargo: number } {
    let cargo = 0;
    let path = 0;
    for (const r of runtimes) {
      if (r.binary.includes('.cargo/bin')) cargo += 1;
      else path += 1;
    }
    return { path, cargo };
  }

  function runtimeLogo(r: Runtime): { hex: string; path: string } | null {
    const key = r.display.toLowerCase();
    if (key.includes('node')) return LOGOS.nodedotjs;
    if (key === 'npm') return LOGOS.npm;
    if (key.includes('python')) return LOGOS.python;
    if (key.includes('rust') || key.includes('cargo')) return LOGOS.rust;
    return null;
  }

  function initials(display: string): string {
    return display.replace(/[^A-Za-z]/g, '').slice(0, 2).toUpperCase() || '??';
  }

  function parkedPaths(): { dir: string; count: number }[] {
    const counts = new Map<string, number>();
    for (const p of projects) {
      if (p.source !== 'Parked') continue;
      const dir = p.path.replace(/\/[^/]+\/?$/, '') || p.path;
      counts.set(dir, (counts.get(dir) ?? 0) + 1);
    }
    return [...counts.entries()]
      .map(([dir, count]) => ({ dir, count }))
      .sort((a, b) => a.dir.localeCompare(b.dir));
  }

  function goto(page: string) {
    active = page;
    notice = '';
  }

  function addProject() {
    active = 'Projects';
    showProjectForms = true;
    notice = '';
  }

  function prefillLink(path: string) {
    linkPath = path;
    showProjectForms = true;
    requestAnimationFrame(() => linkDomainInput?.focus());
  }

  function showNotice(text: string) {
    notice = text;
  }

  async function refresh() {
    loading = true;
    error = '';
    try {
      const status = await invoke<{ protocol: string; version: string }>('daemon_status');
      connected = true;
      daemonProtocol = status.protocol;
      const [p, r, s, d] = await Promise.all([
        invoke<{ projects: Project[] }>('list_projects'),
        invoke<{ runtimes: Runtime[] }>('list_runtimes'),
        invoke<{ services: Service[] }>('list_services'),
        invoke<{ domains: Domain[] }>('list_domains'),
      ]);
      projects = p.projects;
      runtimes = r.runtimes;
      services = s.services;
      domains = d.domains;
    } catch (e) {
      connected = false;
      daemonProtocol = '';
      error = fail(e);
      projects = [];
      runtimes = [];
      services = [];
      domains = [];
    } finally {
      loading = false;
    }
  }

  async function startDaemon() {
    starting = true;
    error = '';
    notice = '';
    try {
      const res = await invoke<{ started: boolean }>('start_daemon');
      notice = res.started ? 'Daemon started.' : 'Daemon was already running.';
      await refresh();
    } catch (e) {
      error = fail(e);
    } finally {
      starting = false;
    }
  }

  async function doScan() {
    busy = 'scan';
    error = '';
    notice = '';
    try {
      const res = await invoke<{ projects: Project[] }>('scan_projects', { path: scanPath });
      notice = `Scan found ${res.projects.length} project${res.projects.length === 1 ? '' : 's'}.`;
      await refreshLists();
    } catch (e) {
      error = fail(e);
    } finally {
      busy = '';
    }
  }

  async function refreshLists() {
    try {
      const [p, r, s, d] = await Promise.all([
        invoke<{ projects: Project[] }>('list_projects'),
        invoke<{ runtimes: Runtime[] }>('list_runtimes'),
        invoke<{ services: Service[] }>('list_services'),
        invoke<{ domains: Domain[] }>('list_domains'),
      ]);
      projects = p.projects;
      runtimes = r.runtimes;
      services = s.services;
      domains = d.domains;
    } catch (e) {
      error = fail(e);
    }
  }

  async function doPark() {
    busy = 'park';
    error = '';
    notice = '';
    try {
      await invoke('park_project', { path: parkPath });
      notice = 'Project parked.';
      parkPath = '';
      await refreshLists();
    } catch (e) {
      error = fail(e);
    } finally {
      busy = '';
    }
  }

  async function doLink() {
    busy = 'link';
    error = '';
    notice = '';
    try {
      await invoke('link_project', { path: linkPath, domain: linkDomain });
      notice = 'Project linked.';
      linkPath = '';
      linkDomain = '';
      await refreshLists();
    } catch (e) {
      error = fail(e);
    } finally {
      busy = '';
    }
  }

  async function doToggleService(s: Service) {
    // Guard against double activation instead of disabling the button:
    // disabling a focused button moves focus to <body>, and the E2E run
    // showed focus never comes back after Start turns into Stop.
    if (busy) return;
    const action = s.status === 'running' ? 'stop_service' : 'start_service';
    busy = `${action === 'start_service' ? 'start' : 'stop'}-${s.name}`;
    error = '';
    notice = '';
    try {
      await invoke(action, { name: s.name });
      notice = `Service ${s.name} ${action === 'start_service' ? 'started' : 'stopped'}.`;
      await refreshLists();
    } catch (e) {
      error = fail(e);
    } finally {
      busy = '';
    }
  }

  onMount(() => {
    refresh();
  });
</script>

<svelte:head>
  <title>Universal Herd</title>
  <meta name="description" content="One environment. Infinite extensions." />
</svelte:head>

<div class="shell">
  <aside class="sidebar">
    <div class="brand">
      <img class="brand-logo" src="/herd-logo.svg" alt="Universal Herd logo" />
      <div><strong>universal herd</strong><span>DEVELOPER ENVIRONMENT</span></div>
    </div>
    <div class="workspace-label">WORKSPACE</div>
    <nav>
      {#each navigation as label}
        <button class:active={active === label} onclick={() => goto(label)}>
          <span class="nav-icon">{@html NAV_ICONS[label]}</span>{label}
        </button>
      {/each}
    </nav>
    <div class="sidebar-bottom">
      <div class="sidebar-foot"><strong>Meow Labs</strong><span>v0.1.0</span></div>
    </div>
  </aside>

  <main>
    <header class="topbar">
      <div class="daemon-status">
        <span class="status-dot" class:on={connected} class:off={!connected}></span>
        <div>
          <strong>{connected ? 'Daemon Connected' : 'Daemon Disconnected'}</strong>
          <span>{connected ? 'Local Machine · 127.0.0.1:47831' : 'Daemon not running · 127.0.0.1:47831'}</span>
        </div>
      </div>
      <div class="top-actions">
        <button class="btn-secondary" onclick={refresh} disabled={!connected || loading}>{loading ? 'Refreshing…' : 'Refresh'}</button>
        <button class="btn-secondary" onclick={() => goto('Settings')}>Manage</button>
        <button class="icon-button" aria-label="Notifications" title="Notifications" onclick={() => showNotice('No notifications yet.')}><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9" /><path d="M10.3 21a1.94 1.94 0 0 0 3.4 0" /></svg></button>
        <button class="avatar-button" aria-label="User menu" title="Meow Labs, local machine" onclick={() => showNotice('Signed in locally as Meow Labs.')}>U</button>
      </div>
    </header>

    <section class="content">
      <div class="page-heading">
        <div>
          <div class="eyebrow">LOCAL DEVELOPMENT PLATFORM</div>
          <h1>{active}</h1>
          <p>{pageMeta[active].sub}</p>
        </div>
        <div class="heading-actions">
          {#if active === 'Containers' || active === 'Plugins'}
            <span class="ds-tag">DESIGNED STATE</span>
          {/if}
          {#if active === 'Overview' || active === 'Projects'}
            <button class="btn-primary" onclick={addProject}>+ Add project</button>
          {:else if active === 'Domains'}
            <button class="btn-primary" onclick={addProject}>Add a local domain</button>
          {:else if active === 'Containers'}
            <button class="btn-primary" disabled title="Container runtimes are not connected yet.">Connect a runtime</button>
          {:else if active === 'Plugins'}
            <button class="btn-primary" disabled title="The plugin registry is not available yet.">Browse plugins</button>
          {/if}
        </div>
      </div>

      {#if !connected && !loading}
        <div class="banner banner-disconnected">
          <div class="banner-body">
            <span class="banner-icon"><svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 22v-5" /><path d="M9 8V2" /><path d="M15 8V2" /><path d="M18 8v5a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V8Z" /></svg></span>
            <div>
              <strong>Not connected to daemon</strong>
              <span>The daemon is not running, so this screen shows no live data.</span>
            </div>
          </div>
          <div class="banner-actions">
            <button class="btn-primary" onclick={startDaemon} disabled={starting}>{starting ? 'Starting...' : 'Start daemon'}</button>
            <button class="btn-secondary" onclick={refresh} disabled={loading}>Connect</button>
          </div>
        </div>
      {/if}
      {#if error}
        <div class="banner banner-error">
          <div class="banner-body">
            <div><strong>Something went wrong.</strong><span>{error}</span></div>
          </div>
        </div>
      {/if}
      {#if notice}
        <div class="banner banner-notice">
          <div class="banner-body"><span>{notice}</span></div>
        </div>
      {/if}

      {#if active === 'Overview'}
        {#if connected}
          <div class="banner banner-connected">
            <div class="banner-body">
              <span class="banner-icon">✓</span>
              <div>
                <strong>Connected · Local machine</strong>
                <span>Daemon running on 127.0.0.1:47831 · protocol {daemonProtocol} · token mode 0600</span>
              </div>
            </div>
            <div class="banner-actions">
              <button class="btn-secondary" onclick={() => goto('Settings')}>Daemon settings</button>
            </div>
          </div>
        {/if}
        <div class="hero">
          <div>
            <h2>Universal Herd</h2>
            <p class="tagline">One environment. Infinite extensions.</p>
            <p>Manage your local development environment with power, flexibility, and freedom.</p>
            <div class="hero-actions">
              <button class="btn-primary" onclick={() => goto('Projects')}>Get Started</button>
              <button class="btn-secondary" onclick={() => goto('Domains')}>Learn More</button>
            </div>
          </div>
          <div class="hero-art"><img src="/herd-logo.svg" alt="Universal Herd logo" /></div>
        </div>
        <div class="section-head">
          <h3><span class="sec-num">01</span>Environment Overview</h3>
          <span class="sec-side">{connected ? `${projects.length} project${projects.length === 1 ? '' : 's'} · ${runtimes.length} runtime${runtimes.length === 1 ? '' : 's'}` : 'DAEMON NOT CONNECTED'}</span>
        </div>
        <div class="stat-grid">
          <button class="stat-card" onclick={() => goto('Projects')}>
            <div class="stat-top"><span>PROJECTS</span><span class="chev">›</span></div>
            <strong>{connected ? projects.length : 'n/a'}</strong>
            <small>{connected ? (projects.length === 0 ? 'No projects yet' : projectKindSummary()) : 'Daemon not connected'}</small>
            <span class="stat-icon">{@html NAV_ICONS.Projects}</span>
          </button>
          <button class="stat-card" onclick={() => goto('Runtimes')}>
            <div class="stat-top"><span>RUNTIMES</span><span class="chev">›</span></div>
            <strong>{connected ? runtimes.length : 'n/a'}</strong>
            <small>{connected ? (runtimes.length === 0 ? 'No runtimes configured' : runtimeNameSummary()) : 'Daemon not connected'}</small>
            <span class="stat-icon">{@html NAV_ICONS.Runtimes}</span>
          </button>
          <button class="stat-card" onclick={() => goto('Services')}>
            <div class="stat-top"><span>SERVICES</span><span class="chev">›</span></div>
            <strong>{connected ? runningServices() : 'n/a'}</strong>
            <small>{connected ? (services.length === 0 ? 'No services running' : `${runningServices()} of ${services.length} service${services.length === 1 ? '' : 's'} running`) : 'Daemon not connected'}</small>
            <span class="stat-icon">{@html ACTIVITY_ICON}</span>
          </button>
          <button class="stat-card" onclick={() => goto('Domains')}>
            <div class="stat-top"><span>LOCAL DOMAINS</span><span class="chev">›</span></div>
            <strong>{connected ? domains.length : 'n/a'}</strong>
            <small>{connected ? (domains.length === 0 ? 'No domains configured' : `${domains.length} domain${domains.length === 1 ? '' : 's'} configured`) : 'Daemon not connected'}</small>
            <span class="stat-icon">{@html NAV_ICONS.Domains}</span>
          </button>
        </div>
        <div class="panel-grid" style="margin-top: 16px;">
          <div class="panel">
            <div class="panel-head"><h3>Get started</h3><span class="panel-side">{doneSteps()} OF 3 DONE</span></div>
            <div class="step-row">
              <span class="step-circle" class:done={runtimes.length > 0}>{runtimes.length > 0 ? '✓' : '1'}</span>
              <div class="step-body"><strong>Configure runtimes</strong><small>{runtimes.length > 0 ? `${runtimes.length} runtimes detected on this machine.` : 'No runtimes detected yet.'}</small></div>
              <button class="btn-small" onclick={() => goto('Runtimes')}>View Runtimes</button>
            </div>
            <div class="step-row">
              <span class="step-circle" class:done={projects.length > 0}>{projects.length > 0 ? '✓' : '2'}</span>
              <div class="step-body"><strong>Create or import a project</strong><small>{projects.length > 0 ? `${projects.length} project${projects.length === 1 ? '' : 's'} already in your workspace.` : 'Add your first project to get started.'}</small></div>
              <button class="btn-small" onclick={() => goto('Projects')}>Open Projects</button>
            </div>
            <div class="step-row">
              <span class="step-circle" class:done={runningServices() > 0}>{runningServices() > 0 ? '✓' : '3'}</span>
              <div class="step-body"><strong>Run the project</strong><small>{runningServices() > 0 ? `${runningServices()} service${runningServices() === 1 ? '' : 's'} running.` : 'Start a service, then open the project.'}</small></div>
              <button class="btn-small" onclick={() => goto('Services')}>View Services</button>
            </div>
          </div>
          {#if connected}
            <div class="panel">
              <div class="panel-head"><h3>Recent Activity</h3></div>
              <p class="panel-sub">Example events, not live data.</p>
              <div class="activity-row"><span class="activity-icon">{@html ACTIVITY_ICON}</span><span>Service postgresql started</span><time>2m ago</time></div>
              <div class="activity-row">
                <span class="activity-icon"><svg viewBox="0 0 24 24"><path d={LOGOS.nodedotjs.path} fill={LOGOS.nodedotjs.hex} /></svg></span>
                <span>Project ui detected</span><time>5m ago</time>
              </div>
              <div class="activity-row"><span class="activity-icon"><img src="/herd-logo.svg" alt="" /></span><span>Daemon connected</span><time>12m ago</time></div>
              <div class="activity-row"><span class="activity-icon">{@html NAV_ICONS.Containers}</span><span>Container herd-dev created</span><time>1h ago</time></div>
            </div>
          {:else}
            <div class="panel">
              <div class="panel-head"><h3>Environment</h3></div>
              <div class="env-row"><span class="env-num">1</span><strong>Machine</strong><small>Local machine</small></div>
              <div class="env-row"><span class="env-num">2</span><strong>Daemon</strong><small>Not running</small></div>
              <div class="env-row"><span class="env-num">3</span><strong>Protocol</strong><small>JSON-RPC 2.0 · version 1.2</small></div>
              <div class="env-row"><span class="env-num">4</span><strong>Token</strong><small>daemon.token · mode 0600</small></div>
            </div>
          {/if}
        </div>
      {:else if active === 'Projects'}
        <div class="toolbar">
          {#each ['All', ...projectKinds()] as kind}
            <button class="seg" class:active={projectFilter === kind} onclick={() => { projectFilter = kind; }}>{kind}</button>
          {/each}
          <input class="search-input" type="text" placeholder="⌕  Search projects..." bind:value={projectSearch} aria-label="Search projects" />
          <span class="sec-side" style="margin-left: auto;">{connected ? `${projects.length} PROJECT${projects.length === 1 ? '' : 'S'} · ${projectKindSummary().toUpperCase() || 'NONE'}` : ''}</span>
        </div>
        {#if showProjectForms}
          <div class="forms-panel">
            <h4>Add a project</h4>
            <p>Scan a directory, park a project, or link one to a local domain. Scan results are not stored, park a project to see it in the table below.</p>
            <div class="form-row">
              <span class="row-label">SCAN</span>
              <input type="text" placeholder="Directory to scan (empty for home)" bind:value={scanPath} aria-label="Directory to scan" />
              <button class="btn-small" onclick={doScan} disabled={busy === 'scan' || !connected}>{busy === 'scan' ? 'Scanning...' : 'Scan'}</button>
            </div>
            <div class="form-row">
              <span class="row-label">PARK</span>
              <input type="text" placeholder="Project path to park" bind:value={parkPath} aria-label="Project path to park" />
              <button class="btn-small" onclick={doPark} disabled={busy === 'park' || !connected || !parkPath}>Park</button>
            </div>
            <div class="form-row">
              <span class="row-label">LINK</span>
              <input type="text" placeholder="Project path to link" bind:value={linkPath} aria-label="Project path to link" />
              <input type="text" placeholder="Domain, for example myapp" bind:value={linkDomain} bind:this={linkDomainInput} aria-label="Domain to link" />
              <button class="btn-small" onclick={doLink} disabled={busy === 'link' || !connected || !linkPath || !linkDomain}>Link</button>
              <button class="btn-small" onclick={() => { showProjectForms = false; }}>Hide</button>
            </div>
          </div>
        {/if}
        {#if loading}
          <div class="skeleton"><i style="width: 40%;"></i><i style="width: 85%;"></i><i style="width: 70%;"></i><i style="width: 90%;"></i></div>
        {:else if connected && filteredProjects().length === 0}
          <div class="empty-state">
            <div class="empty-icon">▦</div>
            <h2>No projects yet.</h2>
            <p>Scan a directory above, or park a project to see it here.</p>
            <button class="btn-secondary" onclick={() => { showProjectForms = true; }}>Add a project</button>
          </div>
        {:else if connected}
          <div class="table-wrap">
            <table class="data-table">
              <thead><tr><th>Name</th><th>Kind</th><th>Runtime</th><th>Domain</th><th>Source</th><th>Path</th><th>Action</th></tr></thead>
              <tbody>
                {#each filteredProjects() as p (p.path)}
                  <tr>
                    <td><strong>{p.name}</strong></td>
                    <td><span class="kind-tag">{p.kind}</span></td>
                    <td>{p.runtime ? `${p.runtime.runtime} ${p.runtime.version}` : 'Not pinned'}</td>
                    <td>{p.domain ?? '-'}</td>
                    <td>{p.source}</td>
                    <td class="mono-cell">{p.path}</td>
                    <td>{#if p.domain}—{:else}<button class="text-action" onclick={() => prefillLink(p.path)}>Link</button>{/if}</td>
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
          <div class="pagination">
            <p>Showing 1–{filteredProjects().length} of {filteredProjects().length} projects</p>
            <div class="pager"><button disabled aria-label="Previous page">‹</button><span>1 / 1</span><button disabled aria-label="Next page">›</button></div>
          </div>
        {/if}
      {:else if active === 'Runtimes'}
        <div class="runtime-top">
          <div class="panel">
            <div class="panel-head"><h3>Detection source</h3></div>
            <p class="panel-sub">Where each runtime was found</p>
            {#if runtimes.length === 0}
              <p class="panel-sub">No runtimes detected yet.</p>
            {:else}
              {@const src = runtimeSourceCounts()}
              {@const max = Math.max(src.path, src.cargo, 1)}
              <div class="bar-row"><span class="bar-label">PATH</span><div class="bar-track"><div class="bar-fill dark" style="width: {(src.path / max) * 100}%;"></div></div><span class="bar-count">{src.path}</span></div>
              <div class="bar-row"><span class="bar-label">~/.cargo/bin</span><div class="bar-track"><div class="bar-fill accent" style="width: {(src.cargo / max) * 100}%;"></div></div><span class="bar-count">{src.cargo}</span></div>
              <p class="panel-sub" style="margin-top: 12px; margin-bottom: 0;">{src.path} from PATH · {src.cargo} from ~/.cargo/bin</p>
            {/if}
          </div>
          <div class="panel">
            <div class="panel-head"><h3>Detected providers</h3></div>
            {#if runtimes.length === 0}
              <p class="panel-sub">No runtimes detected yet.</p>
            {:else}
              {#each runtimes as r (r.name)}
                {@const logo = runtimeLogo(r)}
                <div class="provider-row">
                  <span class="provider-logo">
                    {#if logo}<svg viewBox="0 0 24 24"><path d={logo.path} fill={logo.hex} /></svg>
                    {:else}<span class="initials">{initials(r.display)}</span>{/if}
                  </span>
                  <div><strong>{r.display}</strong><small>{r.version}</small></div>
                </div>
              {/each}
            {/if}
          </div>
        </div>
        {#if connected && runtimes.length === 0}
          <div class="empty-state">
            <div class="empty-icon">⌘</div>
            <h2>No runtimes detected.</h2>
            <p>The daemon found no known runtime binary on PATH. Install one and press Refresh.</p>
          </div>
        {:else if connected}
          <div class="table-wrap">
            <table class="data-table">
              <thead><tr><th>Runtime</th><th>Version</th><th>Location</th><th>Installed</th><th>Action</th></tr></thead>
              <tbody>
                {#each runtimes as r (r.name)}
                  <tr>
                    <td><strong>{r.display}</strong></td>
                    <td>{r.version}</td>
                    <td class="mono-cell">{r.binary}</td>
                    <td><span class="status-cell"><span class="dot green"></span>Installed</span></td>
                    <td>-</td>
                  </tr>
                {/each}
                <tr>
                  <td><strong>PHP</strong></td>
                  <td>8.3</td>
                  <td class="mono-cell">-</td>
                  <td><span class="status-cell"><span class="dot gray"></span>Not installed</span></td>
                  <td><button class="text-action" disabled title="Install from the CLI.">Install</button></td>
                </tr>
                <tr>
                  <td><strong>Go</strong></td>
                  <td>1.23</td>
                  <td class="mono-cell">-</td>
                  <td><span class="status-cell"><span class="dot gray"></span>Not installed</span></td>
                  <td><button class="text-action" disabled title="Install from the CLI.">Install</button></td>
                </tr>
              </tbody>
            </table>
          </div>
          <div class="note-panel info">Runtimes are detected by scanning PATH and ~/.cargo/bin. PHP 8.3 and Go 1.23 are examples of versions you can install; they are not installed yet.</div>
        {/if}
      {:else if active === 'Services'}
        {#if connected && services.length === 0}
          <div class="empty-state">
            <div class="empty-icon">◉</div>
            <h2>No services configured.</h2>
            <p>No managed services yet. Add one from the CLI with herd services create, then start it here.</p>
          </div>
        {:else if connected}
          <div class="section-head"><h3><span class="sec-num">01</span>Services</h3><span class="sec-side">{services.length} SERVICE{services.length === 1 ? '' : 'S'}</span></div>
          <div class="table-wrap">
            <table class="data-table">
              <thead><tr><th>Service</th><th>Command</th><th>Status</th><th>Port</th><th>Action</th></tr></thead>
              <tbody>
                {#each services as s (s.name)}
                  <tr>
                    <td><strong>{s.name}</strong></td>
                    <td class="mono-cell">{s.command}</td>
                    <td>
                      <span class="status-cell">
                        <span class="dot" class:green={s.status === 'running'} class:gray={s.status !== 'running' && s.status !== 'failed'} class:red={s.status === 'failed'}></span>
                        {s.status === 'running' ? 'Running' : s.status === 'failed' ? 'Failed' : 'Stopped'}
                      </span>
                    </td>
                    <td>{s.port ?? '-'}</td>
                    <td>
                      <button
                        class="text-action"
                        onclick={() => doToggleService(s)}
                        aria-disabled={busy === `start-${s.name}` || busy === `stop-${s.name}`}
                      >{busy === `start-${s.name}` ? 'Starting…' : busy === `stop-${s.name}` ? 'Stopping…' : s.status === 'running' ? 'Stop' : 'Start'}</button>
                    </td>
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
        {/if}
      {:else if active === 'Domains'}
        <div class="section-head"><h3><span class="sec-num">01</span>Local domains</h3></div>
        {#if connected && domains.length === 0}
          <div class="empty-state">
            <div class="empty-icon">◎</div>
            <h2>No domains configured.</h2>
            <p>Link a project to a domain from the Projects page to see it here.</p>
          </div>
        {:else if connected}
          <div class="table-wrap">
            <table class="data-table">
              <thead><tr><th>Domain</th><th>Project</th><th>SSL</th><th>Expires</th><th>Action</th></tr></thead>
              <tbody>
                {#each domains as d (d.domain)}
                  <tr>
                    <td><strong>{d.domain}</strong></td>
                    <td class="mono-cell">{d.project}</td>
                    <td>
                      {#if d.secured}<span class="status-cell"><span class="dot green"></span>Secured</span>
                      {:else}<span class="status-cell"><span class="dot amber"></span>Not secured</span>{/if}
                    </td>
                    <td>{d.expires_at ?? '-'}</td>
                    <td>{#if d.secured}-{:else}<button class="text-action" disabled title="Secure from the CLI: herd domains secure {d.domain}">Secure</button>{/if}</td>
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
        {/if}
        <div class="panel">
          <div class="panel-head"><h3>How local HTTPS works</h3></div>
          <div class="env-row"><span class="env-num">1</span><strong>Link a project</strong><small>Map a folder to a name like app.test.</small></div>
          <div class="env-row"><span class="env-num">2</span><strong>Secure the domain</strong><small>Run herd domains secure name.test in a terminal to create a local certificate.</small></div>
          <div class="env-row"><span class="env-num">3</span><strong>Open https://name.test</strong><small>Trust the local CA in your browser once, then no warning.</small></div>
        </div>
        <div class="note-panel info" style="margin-top: 16px;">Certificates are created locally with the daemon's own CA (OpenSSL). No data leaves this machine.</div>
      {:else if active === 'Containers'}
        <div class="note-panel info">No container runtime is connected yet. Connect Docker or Podman to list containers running on this machine.</div>
        <div class="empty-state">
          <div class="empty-illustration">
            <div class="stack-bars">
              <div class="stack-bar"><i></i><b></b></div>
              <div class="stack-bar"><i></i><b></b></div>
              <div class="stack-bar"><i></i><b></b></div>
            </div>
          </div>
          <h2>No containers yet</h2>
          <p>Docker or Podman is not connected yet, so no containers are listed here. Connect a container runtime to see and manage local containers.</p>
          <button class="btn-primary" disabled title="Container runtimes are not connected yet.">Connect a runtime</button>
        </div>
      {:else if active === 'Plugins'}
        <div class="note-panel info">Plugins are third-party extensions. Only install plugins from sources you trust.</div>
        <div class="empty-state">
          <div class="empty-illustration">
            <div class="tile-grid">
              <span class="tile">✦</span><span class="tile">▣</span>
              <span class="tile">▤</span><span class="tile dashed"></span>
            </div>
          </div>
          <h2>No plugins installed</h2>
          <p>No plugins are installed yet. Plugins extend Herd with new runtimes, services, and commands.</p>
          <button class="btn-primary" disabled title="The plugin registry is not available yet.">Browse plugins</button>
        </div>
      {:else if active === 'Settings'}
        <div class="settings-grid">
          <div class="settings-nav">
            {#each settingsNav as section}
              <button class:active={settingsSection === section} onclick={() => { settingsSection = section; }}>{section}</button>
            {/each}
          </div>
          <div class="settings-cards">
            {#if settingsSection === 'General'}
              <div class="panel">
                <div class="panel-head"><h3>General</h3></div>
                <div class="kv-row"><span class="kv-key">Version</span><span class="kv-val">v0.1.0</span></div>
                <div class="kv-row"><span class="kv-key">Protocol</span><span class="kv-val">{connected ? `JSON-RPC 2.0 · version ${daemonProtocol}` : 'Not connected'}</span></div>
                <div class="kv-row"><span class="kv-key">Machine</span><span class="kv-val">Local machine · 127.0.0.1:47831</span></div>
              </div>
            {:else if settingsSection === 'Daemon'}
              <div class="panel">
                <div class="panel-head"><h3>Daemon</h3></div>
                <div class="kv-row"><span class="kv-key">Status</span><span class="kv-val">{connected ? 'Connected' : 'Not connected'}</span></div>
                <div class="kv-row"><span class="kv-key">Address</span><span class="kv-val mono">127.0.0.1:47831</span></div>
                <div class="kv-row"><span class="kv-key">Token</span><span class="kv-val mono">daemon.token · mode 0600</span></div>
                {#if !connected}
                  <div style="margin-top: 16px;"><button class="btn-primary" onclick={startDaemon} disabled={starting}>{starting ? 'Starting...' : 'Start daemon'}</button></div>
                {/if}
              </div>
              <div class="panel">
                <div class="panel-head"><h3>Appearance</h3></div>
                <div class="kv-row"><span class="kv-key">Theme</span><span class="kv-val">Light</span></div>
                <p class="panel-sub" style="margin-top: 10px; margin-bottom: 0;">Dark and System themes are not designed yet.</p>
              </div>
            {:else if settingsSection === 'Runtimes'}
              <div class="panel">
                <div class="panel-head"><h3>Runtimes</h3></div>
                {#if runtimes.length === 0}
                  <p class="panel-sub">No runtimes detected yet.</p>
                {:else}
                  {#each runtimes as r (r.name)}
                    <div class="kv-row"><span class="kv-key">{r.display}</span><span class="kv-val mono">{r.version}</span></div>
                  {/each}
                {/if}
              </div>
            {:else if settingsSection === 'Domains'}
              <div class="panel">
                <div class="panel-head"><h3>Domains</h3></div>
                {#if domains.length === 0}
                  <p class="panel-sub">No domains configured yet.</p>
                {:else}
                  {#each domains as d (d.domain)}
                    <div class="kv-row"><span class="kv-key mono">{d.domain}</span><span class="kv-val">{d.secured ? 'Secured' : 'Not secured'}</span></div>
                  {/each}
                {/if}
              </div>
            {:else if settingsSection === 'Parked Paths'}
              <div class="panel">
                <div class="panel-head"><h3>Parked Paths</h3></div>
                <p class="panel-sub">Folders whose subfolders are served as name.test.</p>
                {#if parkedPaths().length === 0}
                  <p class="panel-sub">No parked paths yet.</p>
                {:else}
                  {#each parkedPaths() as pp (pp.dir)}
                    <div class="kv-row">
                      <span class="kv-key mono">{pp.dir}</span>
                      <span class="kv-val">{pp.count} project{pp.count === 1 ? '' : 's'}</span>
                      <button class="text-action" style="margin-left: auto;" disabled title="Unpark from the CLI.">Forget</button>
                    </div>
                  {/each}
                {/if}
                <div style="margin-top: 16px;"><button class="btn-secondary" disabled title="Park a project from the Projects page.">Add path</button></div>
              </div>
            {/if}
          </div>
        </div>
      {/if}
      <footer><span>© 2026 Meow Labs</span></footer>
    </section>
  </main>
</div>
