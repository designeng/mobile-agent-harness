// Harness dashboard: Vue 3 (global build, no bundler). Polls the JSON API served by `harness dashboard`.
const { createApp, ref, computed, watch, onMounted, onUnmounted } = Vue;

const POLL_MS = 5000;

const COLUMNS = [
  { id: 'backlog', title: 'Backlog', hint: 'Открытые issues без ready-лейбла' },
  { id: 'blocked', title: 'Blocked', hint: 'Ready, но заблокированы лейблом или открытыми зависимостями' },
  { id: 'ready', title: 'Ready', hint: 'Очередь возьмёт их следующими' },
  { id: 'running', title: 'Running', hint: 'Агенты работают сейчас' },
  { id: 'paused', title: 'Paused', hint: 'Остановлены (окружение или падение процесса) — продолжатся с последнего шага' },
  { id: 'failed', title: 'Failed', hint: 'Harness не смог закончить' },
  { id: 'done', title: 'Done', hint: 'Закоммичено / открыт PR' },
];

async function getJson(url) {
  const r = await fetch(url);
  const body = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(body.error || `${r.status} ${r.statusText}`);
  return body;
}

const money = (v) => (v == null ? '—' : `$${Number(v).toFixed(2)}`);

function duration(secs) {
  if (secs == null) return '—';
  const s = Math.round(secs);
  if (s < 90) return `${s}s`;
  if (s < 5400) return `${Math.round(s / 60)}m`;
  if (s < 172800) return `${(s / 3600).toFixed(1)}h`;
  return `${Math.round(s / 86400)}d`;
}

function between(a, b) {
  if (!a) return null;
  const end = b ? Date.parse(b) : Date.now();
  return (end - Date.parse(a)) / 1000;
}

function when(ts) {
  if (!ts) return '—';
  const d = new Date(ts);
  return d.toLocaleString(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
}

const tokens = (n) => (n == null ? '—' : n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${Math.round(n / 1e3)}k` : String(n));

const StateBadge = {
  props: ['state'],
  template: `<span class="badge" :class="'st-' + state">{{ state }}</span>`,
};

const TaskCard = {
  props: ['task', 'selected'],
  emits: ['open'],
  components: { StateBadge },
  setup(props) {
    const run = computed(() => props.task.run);
    const stage = computed(() => {
      const r = run.value;
      if (!r) return null;
      if (r.state === 'done') return 'finished';
      if (!r.stage) return 'starting';
      const it = r.stage.iter > 0 ? ` #${r.stage.iter}` : '';
      return r.stage.finished ? `${r.stage.label} ✓${it}` : `${r.stage.label}${it}`;
    });
    const otherLabels = computed(() => props.task.labels.filter((l) => !l.startsWith('agent')));
    return { run, stage, otherLabels, money, duration };
  },
  template: `
  <article class="card" :class="{ selected, live: run && run.state === 'running' }" tabindex="0"
           @click="$emit('open', task)" @keydown.enter="$emit('open', task)">
    <header class="card-head">
      <span v-if="task.kind === 'issue'" class="num">#{{ task.number }}</span>
      <span v-else class="num kind">explore</span>
      <span v-if="run && run.complexity" class="cx" :title="'Сложность по оценке planner: ' + run.complexity">{{ run.complexity }}</span>
      <span v-if="!task.open" class="closed" title="Issue закрыт на GitHub">closed</span>
    </header>
    <h3 class="card-title">{{ task.title }}</h3>
    <div v-if="run" class="card-stage">
      <span class="dot" :class="'st-' + run.state"></span>
      <span>{{ stage }}</span>
      <span v-if="run.state === 'running' || run.state === 'paused' || run.state === 'crashed'" class="muted">· {{ duration(run.idle_secs) }} назад</span>
    </div>
    <div v-if="task.open_deps && task.open_deps.length" class="card-deps">ждёт {{ task.open_deps.map(n => '#' + n).join(', ') }}</div>
    <div v-if="run" class="card-meta">
      <span v-if="run.qa" class="pill" :class="run.qa.verdict === 'pass' ? 'ok' : 'bad'">QA {{ run.qa.verdict }}<template v-if="run.qa.bugs"> · {{ run.qa.bugs }} bug</template></span>
      <span v-if="run.review" class="pill" :class="run.review.approve ? 'ok' : 'bad'">{{ run.review.approve ? 'approved' : run.review.blocking + ' blocking' }}</span>
      <span class="cost">{{ money(task.total_cost ?? run.cost) }}</span>
    </div>
    <p v-if="run && run.error && (run.state === 'failed' || run.state === 'paused' || run.state === 'crashed')" class="card-error">{{ run.error }}</p>
    <div v-if="otherLabels.length" class="labels"><span v-for="l in otherLabels" :key="l" class="label">{{ l }}</span></div>
    <a v-if="run && run.pr_url" class="pr" :href="run.pr_url" target="_blank" rel="noopener" @click.stop>PR ↗</a>
  </article>`,
};

const FileViewer = {
  props: ['file'],
  emits: ['close'],
  template: `
  <div class="modal-bg" @click.self="$emit('close')">
    <div class="modal" role="dialog" aria-modal="true">
      <header><code>{{ file.path }}</code><button class="icon" @click="$emit('close')" aria-label="Закрыть">✕</button></header>
      <pre v-if="file.text != null">{{ file.text }}</pre>
      <p v-else-if="file.error" class="error">{{ file.error }}</p>
      <p v-else class="muted">Загрузка…</p>
    </div>
  </div>`,
};

const RunDetail = {
  props: ['task'],
  emits: ['close'],
  components: { StateBadge, FileViewer },
  setup(props) {
    const runId = ref(null);
    const detail = ref(null);
    const error = ref(null);
    const file = ref(null);
    const tab = ref('timeline');
    let timer = null;

    async function load() {
      if (runId.value == null) return;
      try {
        detail.value = await getJson(`/api/runs/${runId.value}`);
        error.value = null;
      } catch (e) {
        error.value = e.message;
      }
    }
    watch(() => props.task.key, () => {
      runId.value = props.task.run_ids[0] ?? null;
      detail.value = null;
      tab.value = 'timeline';
      load();
    }, { immediate: true });
    watch(runId, load);
    onMounted(() => {
      timer = setInterval(() => {
        if (detail.value && detail.value.status === 'running') load();
      }, POLL_MS);
    });
    onUnmounted(() => clearInterval(timer));

    const iterations = computed(() => {
      const d = detail.value;
      if (!d) return [];
      const groups = new Map();
      for (const s of d.stages) {
        if (!groups.has(s.iter)) groups.set(s.iter, []);
        groups.get(s.iter).push({ ...s, calls: d.agent_calls.filter((c) => c.stage_id === s.id) });
      }
      return [...groups.entries()].map(([iter, stages]) => ({ iter, stages }));
    });
    const totals = computed(() => {
      const calls = detail.value ? detail.value.agent_calls : [];
      const sum = (k) => calls.reduce((a, c) => a + (c[k] || 0), 0);
      return { cost: sum('cost_usd'), input: sum('input_tokens'), output: sum('output_tokens'), cacheRead: sum('cache_read_tokens'), turns: sum('num_turns') };
    });
    const runState = computed(() => {
      const r = props.task.run;
      return r && r.id === runId.value ? r.state : detail.value && detail.value.status;
    });

    async function openFile(path) {
      file.value = { path, text: null, error: null };
      try {
        const r = await fetch(`/api/runs/${runId.value}/file?path=${encodeURIComponent(path)}`);
        const t = await r.text();
        if (!r.ok) throw new Error(t);
        if (file.value && file.value.path === path) file.value.text = t;
      } catch (e) {
        if (file.value) file.value.error = e.message;
      }
    }
    const parse = (s) => { try { return JSON.parse(s); } catch { return null; } };
    const stageClass = (s) => (s.finished_at == null ? 'st-running' : s.ok ? 'st-done' : 'st-failed');

    return { runId, detail, error, file, tab, iterations, totals, runState, openFile, parse, stageClass, money, duration, between, when, tokens };
  },
  template: `
  <aside class="drawer" aria-label="Детали задачи">
    <header class="drawer-head">
      <div>
        <div class="drawer-sub">
          <a v-if="task.url" :href="task.url" target="_blank" rel="noopener">#{{ task.number }} ↗</a>
          <span v-else>explore</span>
          <StateBadge v-if="runState" :state="runState" />
        </div>
        <h2>{{ task.title }}</h2>
      </div>
      <button class="icon" @click="$emit('close')" aria-label="Закрыть">✕</button>
    </header>

    <div v-if="!task.run_ids.length" class="drawer-body">
      <p class="muted">Harness ещё не запускался по этой задаче.</p>
      <p v-if="task.open_deps.length">Ждёт открытые зависимости: {{ task.open_deps.map(n => '#' + n).join(', ') }}</p>
      <p class="muted">Запуск: <code>harness run {{ task.number }}</code> или <code>harness queue</code></p>
    </div>

    <div v-else class="drawer-body">
      <label v-if="task.run_ids.length > 1" class="run-select">Прогон
        <select v-model="runId"><option v-for="id in task.run_ids" :key="id" :value="id">#{{ id }}{{ id === task.run_ids[0] ? ' (последний)' : '' }}</option></select>
      </label>
      <p v-if="error" class="error">{{ error }}</p>
      <template v-if="detail">
        <dl class="facts">
          <div><dt>Прогон</dt><dd><code>{{ detail.name }}</code></dd></div>
          <div><dt>Ветка</dt><dd><code>{{ detail.branch || '—' }}</code></dd></div>
          <div><dt>Начат</dt><dd>{{ when(detail.started_at) }}</dd></div>
          <div><dt>Длительность</dt><dd>{{ duration(between(detail.started_at, detail.finished_at)) }}</dd></div>
          <div><dt>Стоимость</dt><dd>{{ money(totals.cost) }}</dd></div>
          <div><dt>Токены in/out</dt><dd>{{ tokens(totals.input) }} / {{ tokens(totals.output) }}</dd></div>
          <div v-if="detail.complexity"><dt>Сложность</dt><dd>{{ detail.complexity }}</dd></div>
          <div v-if="detail.pr_url"><dt>PR</dt><dd><a :href="detail.pr_url" target="_blank" rel="noopener">{{ detail.pr_url.replace(/^https:\\/\\/github.com\\//, '') }}</a></dd></div>
        </dl>
        <pre v-if="detail.error" class="error-box">{{ detail.error }}</pre>
        <p v-if="detail.summary" class="summary">{{ detail.summary }}</p>

        <nav class="tabs" role="tablist">
          <button v-for="t in [['timeline','Этапы'],['agents','Агенты'],['qa','QA (' + detail.qa.length + ')'],['review','Review (' + detail.reviews.length + ')'],['plan','План'],['issue','Issue']]"
                  :key="t[0]" role="tab" :aria-selected="tab === t[0]" :class="{ active: tab === t[0] }" @click="tab = t[0]">{{ t[1] }}</button>
        </nav>

        <section v-if="tab === 'timeline'">
          <p v-if="!iterations.length" class="muted">Этапов пока нет.</p>
          <div v-for="g in iterations" :key="g.iter" class="iter">
            <h4>{{ g.iter === 0 ? 'Вне цикла исправлений' : 'Итерация ' + g.iter }}</h4>
            <ol class="timeline">
              <li v-for="s in g.stages" :key="s.id" :class="stageClass(s)">
                <span class="dot" :class="stageClass(s)"></span>
                <span class="tl-name">{{ s.name }}</span>
                <span class="muted">{{ duration(between(s.started_at, s.finished_at)) }}</span>
                <span v-for="c in s.calls" :key="c.id" class="muted">· {{ money(c.cost_usd) }}</span>
                <button v-if="s.log_path" class="link" @click="openFile(s.log_path)">лог</button>
                <button v-for="c in s.calls" :key="'t' + c.id" class="link" @click="openFile(c.transcript)">transcript</button>
                <div v-if="s.detail" class="tl-detail">{{ s.detail }}</div>
              </li>
            </ol>
          </div>
        </section>

        <section v-if="tab === 'agents'">
          <div class="table-wrap">
            <table>
              <thead><tr><th>Роль</th><th>Модель</th><th class="r">Ходы</th><th class="r">Время</th><th class="r">In</th><th class="r">Out</th><th class="r">Cache</th><th class="r">$</th></tr></thead>
              <tbody>
                <tr v-for="c in detail.agent_calls" :key="c.id" :class="{ 'row-err': c.error }" :title="c.error || ''">
                  <td><button class="link" @click="openFile(c.transcript)">{{ c.role }}</button></td>
                  <td><code>{{ (c.model || '—').replace('claude-', '') }}</code></td>
                  <td class="r">{{ c.num_turns ?? '—' }}</td>
                  <td class="r">{{ c.finished_at ? duration(c.duration_ms / 1000) : 'идёт…' }}</td>
                  <td class="r">{{ tokens(c.input_tokens) }}</td>
                  <td class="r">{{ tokens(c.output_tokens) }}</td>
                  <td class="r">{{ tokens(c.cache_read_tokens) }}</td>
                  <td class="r">{{ money(c.cost_usd) }}</td>
                </tr>
              </tbody>
              <tfoot><tr><td colspan="2">Итого</td><td class="r">{{ totals.turns }}</td><td></td><td class="r">{{ tokens(totals.input) }}</td><td class="r">{{ tokens(totals.output) }}</td><td class="r">{{ tokens(totals.cacheRead) }}</td><td class="r">{{ money(totals.cost) }}</td></tr></tfoot>
            </table>
          </div>
        </section>

        <section v-if="tab === 'qa'">
          <p v-if="!detail.qa.length" class="muted">QA ещё не проводился.</p>
          <div v-for="q in detail.qa.slice().reverse()" :key="q.id" class="block">
            <h4>Итерация {{ q.iter }} · <span :class="q.verdict === 'pass' ? 'ok-t' : 'bad-t'">{{ q.verdict }}</span></h4>
            <p v-if="q.notes" class="muted">{{ q.notes }}</p>
            <ul class="checks">
              <li v-for="(c, i) in q.criteria" :key="i" :class="c.met ? 'ok-t' : 'bad-t'">
                <span>{{ c.met ? '✓' : '✗' }}</span> <span class="fg">{{ c.criterion }}</span>
                <div v-if="c.evidence" class="muted small">{{ c.evidence }}</div>
              </li>
            </ul>
            <div v-for="(b, i) in q.bugs" :key="'b' + i" class="bug">
              <strong>🐞 {{ b.title }}</strong>
              <ol v-if="parse(b.steps_json)" class="small"><li v-for="(st, j) in parse(b.steps_json)" :key="j">{{ st }}</li></ol>
              <div v-if="b.expected" class="small"><b>Ожидалось:</b> {{ b.expected }}</div>
              <div v-if="b.actual" class="small"><b>Фактически:</b> {{ b.actual }}</div>
            </div>
          </div>
        </section>

        <section v-if="tab === 'review'">
          <p v-if="!detail.reviews.length" class="muted">Review ещё не было.</p>
          <div v-for="r in detail.reviews.slice().reverse()" :key="r.id" class="block">
            <h4>Итерация {{ r.iter }} · <span :class="r.approve ? 'ok-t' : 'bad-t'">{{ r.approve ? 'approved' : 'changes requested' }}</span></h4>
            <p v-if="r.pr_title"><b>{{ r.pr_title }}</b></p>
            <ul class="comments"><li v-for="(c, i) in r.comments" :key="i">{{ c.body }}</li></ul>
          </div>
        </section>

        <section v-if="tab === 'plan'"><pre class="md">{{ detail.plan_md || 'Плана пока нет.' }}</pre></section>
        <section v-if="tab === 'issue'"><pre class="md">{{ detail.issue_md || detail.title || '—' }}</pre></section>
      </template>
      <p v-else-if="!error" class="muted">Загрузка…</p>
    </div>
    <FileViewer v-if="file" :file="file" @close="file = null" />
  </aside>`,
};

const App = {
  components: { TaskCard, RunDetail },
  setup() {
    const board = ref(null);
    const error = ref(null);
    const loading = ref(false);
    const query = ref('');
    const showBacklog = ref(readPref('showBacklog', true));
    const showClosed = ref(readPref('showClosed', true));
    const selectedKey = ref(null);
    const lastOk = ref(null);
    let timer = null;

    function readPref(k, d) {
      try { const v = localStorage.getItem('harness.' + k); return v == null ? d : v === '1'; } catch { return d; }
    }
    function writePref(k, v) {
      try { localStorage.setItem('harness.' + k, v ? '1' : '0'); } catch { /* storage unavailable */ }
    }
    watch(showBacklog, (v) => writePref('showBacklog', v));
    watch(showClosed, (v) => writePref('showClosed', v));

    async function refresh() {
      loading.value = true;
      try {
        board.value = await getJson('/api/board');
        error.value = null;
        lastOk.value = new Date();
      } catch (e) {
        error.value = e.message;
      } finally {
        loading.value = false;
      }
    }
    onMounted(() => {
      refresh();
      timer = setInterval(() => { if (!document.hidden) refresh(); }, POLL_MS);
      window.addEventListener('keydown', onKey);
    });
    onUnmounted(() => { clearInterval(timer); window.removeEventListener('keydown', onKey); });
    function onKey(e) { if (e.key === 'Escape') selectedKey.value = null; }

    const tasks = computed(() => {
      if (!board.value) return [];
      const q = query.value.trim().toLowerCase();
      return board.value.tasks.filter((t) => {
        if (!showClosed.value && !t.open) return false;
        if (!q) return true;
        return `#${t.number ?? ''} ${t.title} ${t.labels.join(' ')}`.toLowerCase().includes(q);
      });
    });
    const columns = computed(() => COLUMNS.filter((c) => c.id !== 'backlog' || showBacklog.value)
      .map((c) => ({ ...c, tasks: tasks.value.filter((t) => t.column === c.id) })));
    const counts = computed(() => {
      const all = board.value ? board.value.tasks : [];
      return Object.fromEntries(COLUMNS.map((c) => [c.id, all.filter((t) => t.column === c.id).length]));
    });
    const selected = computed(() => board.value && board.value.tasks.find((t) => t.key === selectedKey.value));
    const activeTask = computed(() => board.value && board.value.tasks.find((t) => t.run && t.run.name === board.value.active_run));

    return { board, error, loading, query, showBacklog, showClosed, selectedKey, selected, columns, counts, lastOk, activeTask, refresh, money, when };
  },
  template: `
  <div class="shell" :class="{ 'has-drawer': selected }">
    <header class="top">
      <div class="brand">
        <h1>Harness</h1>
        <a v-if="board" class="repo" :href="'https://github.com/' + board.repo + '/issues'" target="_blank" rel="noopener">{{ board.repo }}</a>
      </div>
      <div class="kpis" v-if="board">
        <div class="kpi">
          <span class="kpi-label">Сейчас</span>
          <span v-if="activeTask" class="kpi-value live-text" @click="selectedKey = activeTask.key">
            <span class="dot st-running"></span>{{ activeTask.number ? '#' + activeTask.number : 'explore' }} · {{ activeTask.run.stage ? activeTask.run.stage.label : 'starting' }}
          </span>
          <span v-else class="kpi-value muted">простаивает</span>
        </div>
        <div class="kpi"><span class="kpi-label">В очереди</span><span class="kpi-value">{{ counts.ready }}</span></div>
        <div class="kpi"><span class="kpi-label">Требуют внимания</span><span class="kpi-value" :class="{ 'bad-t': counts.failed + counts.paused }">{{ counts.failed + counts.paused }}</span></div>
        <div class="kpi"><span class="kpi-label">Сегодня</span><span class="kpi-value">{{ money(board.cost.today) }}</span></div>
        <div class="kpi"><span class="kpi-label">Всего{{ board.budget_usd ? ' / бюджет' : '' }}</span><span class="kpi-value">{{ money(board.cost.total) }}<template v-if="board.budget_usd"> <span class="muted">/ {{ money(board.budget_usd) }}</span></template></span></div>
      </div>
    </header>

    <div class="toolbar">
      <input v-model="query" type="search" placeholder="Поиск: номер, заголовок, лейбл" aria-label="Поиск задач">
      <label class="toggle"><input type="checkbox" v-model="showBacklog"> Backlog</label>
      <label class="toggle"><input type="checkbox" v-model="showClosed"> Закрытые</label>
      <span class="spacer"></span>
      <span class="muted small" v-if="lastOk">обновлено {{ lastOk.toLocaleTimeString() }}</span>
      <button @click="refresh" :disabled="loading">{{ loading ? '…' : 'Обновить' }}</button>
    </div>

    <p v-if="error" class="banner error">Нет связи с harness: {{ error }}</p>
    <p v-if="board && board.github_error" class="banner warn">GitHub недоступен — показаны только задачи из истории прогонов. <span class="muted small">{{ board.github_error }}</span></p>

    <main class="board" v-if="board">
      <section v-for="col in columns" :key="col.id" class="column" :class="'col-' + col.id">
        <header class="col-head" :title="col.hint">
          <span class="dot" :class="'st-' + col.id"></span>
          <h2>{{ col.title }}</h2>
          <span class="count">{{ col.tasks.length }}</span>
        </header>
        <div class="col-body">
          <TaskCard v-for="t in col.tasks" :key="t.key" :task="t" :selected="t.key === selectedKey" @open="selectedKey = t.key" />
          <p v-if="!col.tasks.length" class="empty">{{ col.hint }}</p>
        </div>
      </section>
    </main>
    <p v-else-if="!error" class="muted pad">Загрузка…</p>

    <RunDetail v-if="selected" :task="selected" @close="selectedKey = null" />
  </div>`,
};

createApp(App).mount('#app');
