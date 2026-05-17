<script lang="ts">
  // Why: The Tauri shell (App.svelte) and the browser shell (WebApp.svelte)
  // render an identical component tree; factoring it here keeps the two
  // entrypoints to a one-line difference and avoids layout duplication.
  // What: Composes Header + SessionList + main panel (SessionDetail when a
  // session is selected, otherwise the global EventFeed), and starts a
  // `refreshSessions` poll loop while mounted.
  // Test: Mount with the daemon up → sessions populate within one poll tick;
  // select a session → the main panel switches from EventFeed to SessionDetail.
  import { onDestroy, onMount } from 'svelte';
  import { activeSessionId, refreshSessions } from '../stores/app';
  import Header from './Header.svelte';
  import SessionList from './SessionList.svelte';
  import SessionDetail from './SessionDetail.svelte';
  import EventFeed from './EventFeed.svelte';

  /** Session poll interval in ms. */
  const POLL_MS = 3000;

  let timer: ReturnType<typeof setInterval> | undefined;

  onMount(() => {
    refreshSessions();
    timer = setInterval(refreshSessions, POLL_MS);
  });

  onDestroy(() => {
    if (timer) clearInterval(timer);
  });
</script>

<div class="flex h-full flex-col">
  <Header />
  <div class="flex min-h-0 flex-1">
    <SessionList />
    <main class="min-h-0 flex-1">
      {#if $activeSessionId}
        <SessionDetail />
      {:else}
        <div class="h-full p-4">
          <EventFeed sessionId={null} />
        </div>
      {/if}
    </main>
  </div>
</div>
