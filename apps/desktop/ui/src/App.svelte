<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';

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
  let connected = $state(false);
  let statusText = $state('Daemon not connected');
  let statusDetail = $state('');
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

  const navigation = [
    { label: 'Overview', icon: '◫' },
    { label: 'Projects', icon: '▦' },
    { label: 'Runtimes', icon: '⌘' },
    { label: 'Services', icon: '◉' },
    { label: 'Domains', icon: '◎' },
  ];

  function fail(e: unknown): string {
    return e instanceof Error ? e.message : String(e);
  }

  function runningServices(): number {
    return services.filter((s) => s.status === 'running').length;
  }

  async function refresh() {
    loading = true;
    error = '';
    try {
      const status = await invoke<{ protocol: string; version: string }>('daemon_status');
      connected = true;
      statusText = 'Daemon connected';
      statusDetail = `protocol ${status.protocol} · v${status.version}`;
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
      statusText = 'Daemon not connected';
      statusDetail = '';
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
      projects = res.projects;
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

  async function doStartService(name: string) {
    busy = `start-${name}`;
    error = '';
    notice = '';
    try {
      await invoke('start_service', { name });
      notice = `Service ${name} started.`;
      await refreshLists();
    } catch (e) {
      error = fail(e);
    } finally {
      busy = '';
    }
  }

  async function doStopService(name: string) {
    busy = `stop-${name}`;
    error = '';
    notice = '';
    try {
      await invoke('stop_service', { name });
      notice = `Service ${name} stopped.`;
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
      {#each navigation as item}
        <button class:active={active === item.label} onclick={() => active = item.label}>
          <span class="nav-icon">{item.icon}</span>{item.label}
        </button>
      {/each}
    </nav>
    <div class="sidebar-bottom">
      <div class="version">EARLY ACCESS <span>v0.1.0</span></div>
      <div class="profile"><div class="avatar">ML</div><div><strong>Meow Labs</strong><span>Local machine</span></div><span class="dots">···</span></div>
    </div>
  </aside>

  <main>
    <header class="topbar">
      <div class="breadcrumb">Workspace <span>/</span> {active}</div>
      <div class="top-actions">
        <span class="status-dot" class:on={connected} class:off={!connected}></span>
        <span>{statusText}</span>
        {#if statusDetail}<span class="status-detail">{statusDetail}</span>{/if}
        {#if !connected}
          <button class="start-button" onclick={startDaemon} disabled={starting}>
            {starting ? 'Starting...' : 'Start daemon'}
          </button>
        {:else}
          <button class="refresh-button" onclick={refresh} disabled={loading}>Refresh</button>
        {/if}
        <button class="icon-button" aria-label="Settings">⚙</button>
      </div>
    </header>
    <section class="content">
      <div class="page-heading"><div><div class="eyebrow">LOCAL DEVELOPMENT PLATFORM</div><h1>{active}</h1><p>Your development environment, in one place.</p></div><button class="primary" onclick={() => active = 'Projects'}>＋ Add project</button></div>

      {#if !connected && !loading}
        <div class="banner">
          <div><strong>Daemon not connected.</strong><span>Start the daemon to see live data. The numbers below are not live data.</span></div>
          <button class="primary" onclick={startDaemon} disabled={starting}>{starting ? 'Starting...' : 'Start daemon'}</button>
        </div>
      {/if}
      {#if error}
        <div class="banner error-banner"><div><strong>Something went wrong.</strong><span>{error}</span></div></div>
      {/if}
      {#if notice}
        <div class="banner notice-banner"><div><span>{notice}</span></div></div>
      {/if}

      {#if active === 'Overview'}
        <div class="hero"><div><div class="hero-kicker">WELCOME TO YOUR WORKSPACE</div><h2>Build without<br />the setup overhead.</h2><p>Manage runtimes, services, and projects from one unified environment.</p><button class="hero-link" onclick={() => active = 'Projects'}>Explore projects <span>↗</span></button></div><div class="hero-art"><div class="orbit orbit-one"></div><div class="orbit orbit-two"></div><div class="core-mark"><img src="/herd-logo.svg" alt="Universal Herd logo" /></div><div class="orbit-node node-one">⌘</div><div class="orbit-node node-two">▦</div><div class="orbit-node node-three">◉</div></div></div>
        <div class="section-head"><div><h3>Environment overview</h3><p>A snapshot of your local setup.</p></div><span class="muted-tag">{connected ? 'LIVE WORKSPACE' : 'DAEMON NOT CONNECTED'}</span></div>
        <div class="stat-grid">
          <article class="stat-card"><div class="stat-top"><span>PROJECTS</span><span>↗</span></div><strong>{connected ? projects.length : 'n/a'}</strong><small>{connected ? `${projects.length} project${projects.length === 1 ? '' : 's'} in workspace` : 'Daemon not connected'}</small></article>
          <article class="stat-card"><div class="stat-top"><span>RUNTIMES</span><span>⌘</span></div><strong>{connected ? runtimes.length : 'n/a'}</strong><small>{connected ? 'Runtimes detected on this machine' : 'Daemon not connected'}</small></article>
          <article class="stat-card"><div class="stat-top"><span>SERVICES</span><span>◉</span></div><strong>{connected ? runningServices() : 'n/a'}</strong><small>{connected ? `${runningServices()} of ${services.length} service${services.length === 1 ? '' : 's'} running` : 'Daemon not connected'}</small></article>
          <article class="stat-card"><div class="stat-top"><span>LOCAL DOMAINS</span><span>◎</span></div><strong>{connected ? domains.length : 'n/a'}</strong><small>{connected ? `${domains.length} domain${domains.length === 1 ? '' : 's'} configured` : 'Daemon not connected'}</small></article>
        </div>
        <div class="section-head lower"><div><h3>Get started</h3><p>Set up the essentials for your first project.</p></div></div>
        <div class="setup-list"><div><span class="step-number">01</span><div><strong>Add your first project</strong><small>Bring an existing codebase into your workspace.</small></div><button onclick={() => active = 'Projects'}>Open projects ↗</button></div><div><span class="step-number">02</span><div><strong>Configure a runtime</strong><small>Choose the language versions your projects need.</small></div><button onclick={() => active = 'Runtimes'}>View runtimes ↗</button></div><div><span class="step-number">03</span><div><strong>Start a local service</strong><small>Prepare databases and supporting services.</small></div><button onclick={() => active = 'Services'}>View services ↗</button></div></div>
      {:else if active === 'Projects'}
        <div class="section-head"><div><h3>Projects</h3><p>Scanned, parked, and linked projects on this machine.</p></div></div>
        <div class="form-row">
          <input type="text" placeholder="Directory to scan (empty for home)" bind:value={scanPath} aria-label="Directory to scan" />
          <button class="secondary" onclick={doScan} disabled={busy === 'scan' || !connected}>{busy === 'scan' ? 'Scanning...' : 'Scan'}</button>
        </div>
        <div class="form-row">
          <input type="text" placeholder="Project path to park" bind:value={parkPath} aria-label="Project path to park" />
          <button class="secondary" onclick={doPark} disabled={busy === 'park' || !connected || !parkPath}>Park</button>
          <input type="text" placeholder="Project path to link" bind:value={linkPath} aria-label="Project path to link" />
          <input type="text" placeholder="Domain, for example myapp" bind:value={linkDomain} aria-label="Domain to link" />
          <button class="secondary" onclick={doLink} disabled={busy === 'link' || !connected || !linkPath || !linkDomain}>Link</button>
        </div>
        {#if connected && projects.length === 0}
          <div class="empty-state"><div class="empty-icon">▦</div><h2>No projects yet.</h2><p>Scan a directory above, or park a project to see it here.</p></div>
        {:else if connected}
          <table class="data-table">
            <thead><tr><th>Name</th><th>Path</th><th>Stack</th><th>Runtime pin</th><th>Domain</th><th>Source</th></tr></thead>
            <tbody>
              {#each projects as p}
                <tr>
                  <td><strong>{p.name}</strong></td>
                  <td class="mono-cell">{p.path}</td>
                  <td>{p.kind}</td>
                  <td>{p.runtime ? `${p.runtime.runtime} ${p.runtime.version}` : 'Not pinned'}</td>
                  <td>{p.domain ?? 'Not linked'}</td>
                  <td>{p.source}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        {/if}
      {:else if active === 'Runtimes'}
        <div class="section-head"><div><h3>Runtimes</h3><p>Language runtimes detected on this machine.</p></div></div>
        {#if connected && runtimes.length === 0}
          <div class="empty-state"><div class="empty-icon">⌘</div><h2>No runtimes detected.</h2><p>The daemon found no known runtime binary on PATH. Install one and press Refresh.</p></div>
        {:else if connected}
          <table class="data-table">
            <thead><tr><th>Runtime</th><th>Version</th><th>Binary</th></tr></thead>
            <tbody>
              {#each runtimes as r}
                <tr>
                  <td><strong>{r.display}</strong></td>
                  <td>{r.version}</td>
                  <td class="mono-cell">{r.binary}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        {/if}
      {:else if active === 'Services'}
        <div class="section-head"><div><h3>Services</h3><p>Managed local services. Start and stop them from here.</p></div></div>
        {#if connected && services.length === 0}
          <div class="empty-state"><div class="empty-icon">◉</div><h2>No services configured.</h2><p>No managed services yet. Add one from the CLI with herd services create, then start it here.</p></div>
        {:else if connected}
          <table class="data-table">
            <thead><tr><th>Name</th><th>Command</th><th>Port</th><th>Status</th><th>Action</th></tr></thead>
            <tbody>
              {#each services as s}
                <tr>
                  <td><strong>{s.name}</strong></td>
                  <td class="mono-cell">{s.command}</td>
                  <td>{s.port ?? 'n/a'}</td>
                  <td><span class="pill" class:running={s.status === 'running'} class:failed={s.status === 'failed'}>{s.status}</span></td>
                  <td>
                    {#if s.status === 'running'}
                      <button class="secondary small" onclick={() => doStopService(s.name)} disabled={busy === `stop-${s.name}`}>Stop</button>
                    {:else}
                      <button class="secondary small" onclick={() => doStartService(s.name)} disabled={busy === `start-${s.name}`}>Start</button>
                    {/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        {/if}
      {:else if active === 'Domains'}
        <div class="section-head"><div><h3>Domains</h3><p>Local domains pointing at projects and services.</p></div></div>
        {#if connected && domains.length === 0}
          <div class="empty-state"><div class="empty-icon">◎</div><h2>No domains configured.</h2><p>Link a project to a domain from the Projects page to see it here.</p></div>
        {:else if connected}
          <table class="data-table">
            <thead><tr><th>Domain</th><th>Target</th><th>HTTPS</th></tr></thead>
            <tbody>
              {#each domains as d}
                <tr>
                  <td><strong>{d.domain}</strong></td>
                  <td class="mono-cell">{d.project}</td>
                  <td>{d.secured ? `Secured${d.expires_at ? ` (expires ${d.expires_at})` : ''}` : 'Not secured'}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        {/if}
      {/if}
      <footer><span>UNIVERSAL HERD <span class="footer-sep">/</span> BUILT BY MEOW LABS</span><span>ONE ENVIRONMENT. INFINITE EXTENSIONS.</span></footer>
    </section>
  </main>
</div>
