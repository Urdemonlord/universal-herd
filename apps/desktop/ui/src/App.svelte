<script lang="ts">
  let active = $state('Overview');
  let daemonState = $state<'Not connected' | 'Core foundation'>('Core foundation');
  const navigation = [
    { label: 'Overview', icon: '◫' },
    { label: 'Projects', icon: '▦' },
    { label: 'Runtimes', icon: '⌘' },
    { label: 'Services', icon: '◉' },
    { label: 'Domains', icon: '◎' },
  ];
</script>

<svelte:head>
  <title>Universal Herd</title>
  <meta name="description" content="One environment. Infinite extensions." />
</svelte:head>

<div class="shell">
  <aside class="sidebar">
    <div class="brand">
      <div class="brand-mark">h.</div>
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
    <header class="topbar"><div class="breadcrumb">Workspace <span>/</span> {active}</div><div class="top-actions"><span class="status-dot"></span><span>{daemonState}</span><button class="icon-button" aria-label="Settings">⚙</button></div></header>
    <section class="content">
      <div class="page-heading"><div><div class="eyebrow">LOCAL DEVELOPMENT PLATFORM</div><h1>{active}</h1><p>Your development environment, in one place.</p></div><button class="primary" onclick={() => active = 'Projects'}>＋ Add project</button></div>

      {#if active === 'Overview'}
        <div class="hero"><div><div class="hero-kicker">WELCOME TO YOUR WORKSPACE</div><h2>Build without<br />the setup overhead.</h2><p>Manage runtimes, services, and projects from one unified environment.</p><button class="hero-link" onclick={() => active = 'Projects'}>Explore projects <span>↗</span></button></div><div class="hero-art"><div class="orbit orbit-one"></div><div class="orbit orbit-two"></div><div class="core-mark">h.</div><div class="orbit-node node-one">⌘</div><div class="orbit-node node-two">▦</div><div class="orbit-node node-three">◉</div></div></div>
        <div class="section-head"><div><h3>Environment overview</h3><p>A snapshot of your local setup.</p></div><span class="muted-tag">LIVE WORKSPACE</span></div>
        <div class="stat-grid"><article class="stat-card"><div class="stat-top"><span>PROJECTS</span><span>↗</span></div><strong>0</strong><small>No projects discovered yet</small></article><article class="stat-card"><div class="stat-top"><span>RUNTIMES</span><span>⌘</span></div><strong>0</strong><small>Runtime providers not configured</small></article><article class="stat-card"><div class="stat-top"><span>SERVICES</span><span>◉</span></div><strong>0</strong><small>No managed services</small></article><article class="stat-card"><div class="stat-top"><span>LOCAL DOMAINS</span><span>◎</span></div><strong>0</strong><small>No domains configured</small></article></div>
        <div class="section-head lower"><div><h3>Get started</h3><p>Set up the essentials for your first project.</p></div></div>
        <div class="setup-list"><div><span class="step-number">01</span><div><strong>Add your first project</strong><small>Bring an existing codebase into your workspace.</small></div><button onclick={() => active = 'Projects'}>Open projects ↗</button></div><div><span class="step-number">02</span><div><strong>Configure a runtime</strong><small>Choose the language versions your projects need.</small></div><button onclick={() => active = 'Runtimes'}>View runtimes ↗</button></div><div><span class="step-number">03</span><div><strong>Start a local service</strong><small>Prepare databases and supporting services.</small></div><button onclick={() => active = 'Services'}>View services ↗</button></div></div>
      {:else}
        <div class="empty-state"><div class="empty-icon">{active === 'Projects' ? '▦' : active === 'Runtimes' ? '⌘' : active === 'Services' ? '◉' : '◎'}</div><h2>{active} are coming online.</h2><p>This area is part of the v0.1 foundation. The Rust core and CLI are being established before connecting this view to live system data.</p><button class="secondary" onclick={() => active = 'Overview'}>Back to overview</button></div>
      {/if}
      <footer><span>UNIVERSAL HERD <span class="footer-sep">/</span> BUILT BY MEOW LABS</span><span>ONE ENVIRONMENT. INFINITE EXTENSIONS.</span></footer>
    </section>
  </main>
</div>
