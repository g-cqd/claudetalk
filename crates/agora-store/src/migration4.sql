-- M5: scheduling (work queue, jobs, leases, timers, policies, notices) and card extensions.
ALTER TABLE identities ADD COLUMN system INTEGER NOT NULL DEFAULT 0;
ALTER TABLE cards ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;  -- higher = more urgent
ALTER TABLE cards ADD COLUMN value REAL NOT NULL DEFAULT 1;
ALTER TABLE cards ADD COLUMN size REAL NOT NULL DEFAULT 1;
ALTER TABLE cards ADD COLUMN team TEXT NOT NULL DEFAULT '';
ALTER TABLE cards ADD COLUMN needs_owner INTEGER NOT NULL DEFAULT 0;
ALTER TABLE cards ADD COLUMN owner_action TEXT NOT NULL DEFAULT '';
CREATE TABLE card_deps(card TEXT NOT NULL REFERENCES cards(id), after TEXT NOT NULL REFERENCES cards(id), PRIMARY KEY(card, after));
CREATE TABLE card_links(card TEXT NOT NULL REFERENCES cards(id), rel TEXT NOT NULL, target TEXT NOT NULL, created INTEGER NOT NULL, by TEXT NOT NULL, PRIMARY KEY(card, rel, target));
CREATE TABLE work_scopes(id TEXT PRIMARY KEY, kind TEXT NOT NULL CHECK(kind IN ('project','conversation','agent')), parent TEXT REFERENCES work_scopes(id), task TEXT NOT NULL DEFAULT '', agent TEXT, created INTEGER NOT NULL, updated INTEGER NOT NULL);
CREATE TABLE work_tasks(seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, title TEXT NOT NULL DEFAULT '', kind TEXT NOT NULL DEFAULT 'other',
  scope TEXT REFERENCES work_scopes(id), author TEXT NOT NULL, queued INTEGER NOT NULL, after TEXT NOT NULL DEFAULT '[]',
  size REAL NOT NULL DEFAULT 1 CHECK(size > 0), value REAL NOT NULL DEFAULT 1 CHECK(value > 0), files TEXT NOT NULL DEFAULT '[]',
  command TEXT, cwd TEXT, weight REAL NOT NULL DEFAULT 1 CHECK(weight > 0),
  state TEXT NOT NULL CHECK(state IN ('queued','claimed','running','done','failed','cancelled')),
  agent TEXT, accepted TEXT, claimed INTEGER, requested INTEGER, started INTEGER, ended INTEGER,
  supervisor_pid INTEGER, supervisor_start INTEGER, pid INTEGER, status INTEGER, log TEXT, qos TEXT,
  cpu REAL, rss INTEGER, rss_peak INTEGER, cpu_time REAL, sampled INTEGER, note TEXT NOT NULL DEFAULT '',
  sig_full TEXT, sig_head TEXT, sig_prog TEXT, sig_tool TEXT, prediction TEXT, card TEXT REFERENCES cards(id),
  origin TEXT, exclusive INTEGER NOT NULL DEFAULT 0, quiet_until INTEGER, cancel_requested INTEGER NOT NULL DEFAULT 0);
CREATE INDEX work_tasks_state ON work_tasks(state, seq);
CREATE INDEX work_tasks_sig ON work_tasks(sig_full, sig_head, sig_prog, sig_tool);
CREATE INDEX work_tasks_card ON work_tasks(card);
CREATE TABLE job_history(seq INTEGER PRIMARY KEY AUTOINCREMENT, task TEXT NOT NULL, sig_full TEXT, sig_head TEXT, sig_prog TEXT, sig_tool TEXT,
  weight REAL NOT NULL, status INTEGER, started INTEGER NOT NULL, ended INTEGER NOT NULL, cpu_time REAL, rss_peak INTEGER, requested INTEGER);
CREATE INDEX job_history_full ON job_history(sig_full, ended);
CREATE INDEX job_history_head ON job_history(sig_head, ended);
CREATE INDEX job_history_prog ON job_history(sig_prog, ended);
CREATE INDEX job_history_tool ON job_history(sig_tool, ended);
CREATE TABLE work_settings(id INTEGER PRIMARY KEY CHECK(id = 1), json TEXT NOT NULL);   -- WorkSettings JSON; row absent = defaults
CREATE TABLE pools(name TEXT PRIMARY KEY, capacity REAL NOT NULL CHECK(capacity > 0), unit TEXT NOT NULL DEFAULT '', note TEXT NOT NULL DEFAULT '', created INTEGER NOT NULL, created_by TEXT NOT NULL);
CREATE TABLE leases(id TEXT PRIMARY KEY, kind TEXT NOT NULL CHECK(kind IN ('path','pool','quiet')), resource TEXT NOT NULL, amount REAL NOT NULL DEFAULT 0,
  holder TEXT NOT NULL, card TEXT, task TEXT, created INTEGER NOT NULL, expires INTEGER, heartbeat INTEGER NOT NULL, expected_end INTEGER,
  released INTEGER, note TEXT NOT NULL DEFAULT '');
CREATE INDEX leases_active ON leases(kind, released, expires);
CREATE TABLE timers(id TEXT PRIMARY KEY, owner TEXT NOT NULL, spec TEXT NOT NULL, template TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1,
  next_fire INTEGER, last_fire INTEGER, fired INTEGER NOT NULL DEFAULT 0, created INTEGER NOT NULL);
CREATE INDEX timers_due ON timers(enabled, next_fire);
CREATE TABLE policies(team TEXT NOT NULL, lane TEXT NOT NULL, json TEXT NOT NULL, updated INTEGER NOT NULL, by TEXT NOT NULL, PRIMARY KEY(team, lane));
CREATE TABLE notices(seq INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, target TEXT NOT NULL, kind TEXT NOT NULL, subject TEXT NOT NULL, text TEXT NOT NULL);
CREATE INDEX notices_target ON notices(target, seq);
CREATE TABLE reconcile_marks(key TEXT PRIMARY KEY, last INTEGER NOT NULL, count INTEGER NOT NULL DEFAULT 1);
CREATE TABLE work_state(key TEXT PRIMARY KEY, value TEXT NOT NULL);  -- 'admission_paused' (reason), 'blocker_card' (id)
