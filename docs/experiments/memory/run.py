#!/usr/bin/env python3
"""Disposable PostgreSQL checks. Requires Python 3 and permission to use Docker."""
import concurrent.futures
from fractions import Fraction
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parent
IMAGE = os.environ.get('MEMORY_POSTGRES_IMAGE',
    'postgres@sha256:5c4783a8cc3f0074912164f94dfa1be4e043438f0f707cf968d7af9a0d31a2c8')
NAME = 'elsebot-memory-' + uuid.uuid4().hex[:12]
VOLUME = NAME + '-data'
RESULT = {'status': 'RUNNING', 'container': NAME, 'volume': VOLUME,
          'image_requested': IMAGE, 'checks': []}


def command(*args, data=None):
    p = subprocess.run(args, input=data, text=True, capture_output=True)
    if p.returncode:
        raise RuntimeError(f'{args[0:3]} failed: {p.stderr}')
    return p.stdout.strip()


def sql(statement):
    return command('docker', 'exec', '-i', NAME, 'psql', '-X', '-qAt',
                   '-v', 'ON_ERROR_STOP=1', '-U', 'postgres', data=statement)


def passed(name, **evidence):
    RESULT['checks'].append({'name': name, 'status': 'PASS', **evidence})


def rendered(view, texts):
    return '<chat>\n' + ''.join(
        f'{start}+{span}|{texts[start, span].replace(chr(10), " ")}\n'
        for start, span in view) + '</chat>'


def assert_cover(view, count):
    cursor = 0
    for start, span in view:
        assert start == cursor and start % span == 0
        cursor += span
    assert cursor == count


def merge_one(view, count, built):
    candidates = []
    for pos in range(len(view)-1):
        start, span = view[pos]
        if start % (2*span) == 0 and view[pos+1] == (start+span, span):
            if (start, 2*span) in built:
                due = Fraction(count - (start+2*span-1), span)
                candidates.append((-due, start, pos))
    if not candidates:
        return False
    _, _, pos = min(candidates)
    start, span = view[pos]
    view[pos:pos+2] = [(start, 2*span)]
    return True


def view_experiment():
    rows = json.loads(sql("SELECT json_agg(json_build_array(start_seq,span,body) "
                          "ORDER BY start_seq,span) FROM memory_node WHERE bot_id='tree';"))
    texts = {(start, span): text for start, span, text in rows}
    assert len(texts) == 4095
    regression = [(0, 4), (4, 4), (8, 1), (9, 1)]
    assert merge_one(regression, 10, texts)
    assert regression == [(0, 4), (4, 4), (8, 2)]
    passed('reference_due_regression', count=10, merged='8+1 and 9+1')
    view, batches, compacting = [], [], False
    largest = 0
    for count in range(1, 2049):
        previous = list(view)
        view.append((count-1, 1))
        size = len(rendered(view, texts).encode())
        if size > 128000:
            compacting = True
        changed = False
        if compacting:
            while len(rendered(view, texts).encode()) > 64000:
                if not merge_one(view, count, texts):
                    break
                changed = True
            compacting = len(rendered(view, texts).encode()) > 64000
        if changed:
            batches.append({'count': count, 'bytes': len(rendered(view, texts).encode())})
        else:
            assert view[:-1] == previous
        assert_cover(view, count)
        largest = max(largest, len(rendered(view, texts).encode()))
    assert batches and largest <= 128000
    encoded = json.dumps(view, separators=(',', ':'))
    sql("UPDATE memory_store SET view_nodes='" + encoded + "', published_seq=2047, "
        "view_revision=view_revision+1 WHERE bot_id='tree';")
    before = sql("SELECT view_nodes::text FROM memory_store WHERE bot_id='tree';")
    command('docker', 'restart', NAME)
    ready()
    after = sql("SELECT view_nodes::text FROM memory_store WHERE bot_id='tree';")
    assert before == after
    assert json.loads(after) == [list(x) for x in view]
    passed('batched_view_and_database_restart', messages=2048, nodes=len(texts),
           batches=batches, maximum_post_merge_bytes=largest,
           final_bytes=len(rendered(view, texts).encode()),
           saved_view_sha256=hashlib.sha256(after.encode()).hexdigest())
    # Failures can leave a view above target. They must never cause coverage loss.
    blocked = [(i, 1) for i in range(256)]
    assert not merge_one(blocked, 256, {(i, 1) for i in range(256)})
    assert_cover(blocked, 256)
    passed('unbuilt_parents_keep_coverage', covered=256, merges=0)
    # Copy at watermark 2047 while another connection appends ten new messages.
    original = sql("SELECT encode(sha256(convert_to(string_agg(body,'|' ORDER BY seq),"
                   "'UTF8')),'hex') FROM memory_entry WHERE bot_id='tree';")
    def copy_generation():
        sql("SET application_name='memory-copy'; BEGIN; "
            "INSERT INTO memory_node SELECT bot_id,2,start_seq,span,body,method,input_hash,audit "
            "FROM memory_node WHERE bot_id='tree' AND generation=1; SELECT pg_sleep(2); COMMIT;")
    def append_during_copy():
        active_query = ("SELECT count(*) FROM pg_stat_activity WHERE application_name='memory-copy' "
                        "AND wait_event='PgSleep';")
        for _ in range(40):
            if sql(active_query) == '1':
                break
            time.sleep(0.025)
        else:
            raise RuntimeError('did not observe open rebuild transaction')
        sql("SELECT fixture_append('tree','catchup-'||i,'catch-up message '||i) "
            "FROM generate_series(1,10) i;")
        assert sql(active_query) == '1', 'append did not finish during open rebuild transaction'
    with concurrent.futures.ThreadPoolExecutor(2) as pool:
        futures = [pool.submit(copy_generation), pool.submit(append_during_copy)]
        for future in futures:
            future.result()
    assert sql("SELECT count(*) FROM memory_node WHERE bot_id='tree' AND generation=2;") == '4095'
    assert sql("SELECT next_seq FROM memory_store WHERE bot_id='tree';") == '2058'
    sql("INSERT INTO memory_node(bot_id,generation,start_seq,span,body,method,input_hash) "
        "SELECT bot_id,2,seq,1,rpad('synthetic catch-up '||seq,512,'x'),'synthetic',body_hash "
        "FROM memory_entry WHERE bot_id='tree' AND seq>=2048;")
    view.extend((i,1) for i in range(2048,2058))
    assert_cover(view,2058)
    encoded = json.dumps(view,separators=(',',':'))
    swapped = sql("UPDATE memory_store SET generation=2,published_seq=2057,"
                  "view_nodes='"+encoded+"',view_revision=view_revision+1 "
                  "WHERE bot_id='tree' AND next_seq=2058 RETURNING generation;")
    assert swapped == '2'
    assert original == sql("SELECT encode(sha256(convert_to(string_agg(body,'|' ORDER BY seq),"
                           "'UTF8')),'hex') FROM memory_entry WHERE bot_id='tree' AND seq<2048;")
    passed('synthetic_generation_swap_with_concurrent_append', copied_nodes=4095,
           concurrent_appends=10, published_seq=2057,
           observed_open_copy_transaction_before_and_after_append=True,
           note='Copies synthetic outputs; does not evaluate model regeneration')
    sql("INSERT INTO memory_node SELECT bot_id,3,start_seq,span,body,method,input_hash,audit "
        "FROM memory_node WHERE bot_id='tree' AND generation=1 AND span=1 "
        "AND start_seq<5 AND start_seq<>2;")
    prefix_query = ("SELECT min(e.seq) FROM memory_entry e LEFT JOIN memory_node n "
                    "ON n.bot_id=e.bot_id AND n.generation=3 AND n.span=1 AND n.start_seq=e.seq "
                    "WHERE e.bot_id='tree' AND n.start_seq IS NULL;")
    assert sql(prefix_query) == '2'
    sql("INSERT INTO memory_node SELECT bot_id,3,start_seq,span,body,method,input_hash,audit "
        "FROM memory_node WHERE bot_id='tree' AND generation=1 AND span=1 AND start_seq=2;")
    assert sql(prefix_query) == '5'
    passed('out_of_order_completion_publishes_only_contiguous_prefix', before=2,after=5)


def ready():
    for _ in range(120):
        p = subprocess.run(['docker', 'exec', NAME, 'pg_isready', '-U', 'postgres'],
                           capture_output=True)
        # The image briefly starts a temporary server during initdb. Wait for
        # init to finish so a passing pg_isready cannot race its shutdown.
        logs = subprocess.run(['docker','logs',NAME],capture_output=True,text=True)
        if p.returncode == 0 and 'PostgreSQL init process complete' in logs.stdout:
            return
        time.sleep(0.25)
    raise RuntimeError('dedicated PostgreSQL did not become ready')


def rollback_equivalence():
    def push(state, states):
        if states is None:
            return (0,state,None)
        keep,old,older = states
        return (1,old,older) if keep == 0 else (0,state,push(old,older))
    states, view = None, []
    for count in range(1,20002):
        states = push(count-1,states)
        cursor, starts = states, []
        while cursor:
            _,start,cursor = cursor
            starts.append(start)
        starts.reverse()
        target = [(s,(starts[i+1] if i+1<len(starts) else count)-s)
                  for i,s in enumerate(starts)]
        view.append((count-1,1))
        while len(view)>len(target):
            # All aligned parents are considered built for this algorithm check.
            assert merge_one(view,count,{(s,2*n) for s,n in view})
        assert view == target, (count,view,target)
    passed('rollback_push_equivalence', steps=20001,
           note='Local deterministic algorithm comparison, separate from SQL checks')


def worker(index):
    # Independent connections contend for one bot's row lock. Retried shared key
    # must consume exactly one sequence number across all six connections.
    sql("SELECT fixture_append('concurrent','w"+str(index)+"-'||i,'message '||i) "
        "FROM generate_series(1,20) i; "
        "SELECT fixture_append('concurrent','shared','one retry payload');")


def lease_experiment():
    sql("INSERT INTO memory_job(bot_id,generation,start_seq,span) "
        "VALUES ('tree',2,0,1),('tree',2,1,1);")
    statement = """BEGIN;
      WITH candidate AS (
        SELECT bot_id,generation,start_seq,span FROM memory_job
        WHERE bot_id='tree' AND (lease_until IS NULL OR lease_until < now())
        ORDER BY start_seq,span FOR UPDATE SKIP LOCKED LIMIT 1
      ) UPDATE memory_job j SET lease_token='worker',lease_until=now()+interval '1 minute'
        FROM candidate c WHERE (j.bot_id,j.generation,j.start_seq,j.span)=
          (c.bot_id,c.generation,c.start_seq,c.span) RETURNING j.start_seq;
      SELECT pg_sleep(0.15); COMMIT;"""
    with concurrent.futures.ThreadPoolExecutor(2) as pool:
        claimed = list(pool.map(lambda _: sql(statement).strip(), range(2)))
    assert sorted(claimed) == ['0', '1'], claimed
    sql("UPDATE memory_job SET lease_until=now()-interval '1 second' WHERE bot_id='tree' AND start_seq=0;")
    assert sql("UPDATE memory_job SET lease_token='replacement',lease_until=now()+interval '1 minute' "
               "WHERE bot_id='tree' AND start_seq=0 AND lease_until<now() RETURNING start_seq;") == '0'
    assert sql("DELETE FROM memory_job WHERE bot_id='tree' AND start_seq=0 "
               "AND lease_token='worker' RETURNING start_seq;") == ''
    passed('skip_locked_and_expired_lease_fencing', independently_claimed=claimed,
           stale_completion_rows=0)


volume_created = container_created = False
try:
    command('docker', 'pull', IMAGE)
    RESULT['image_id'] = command('docker', 'image', 'inspect', IMAGE, '--format', '{{.Id}}')
    RESULT['repo_digests'] = json.loads(command('docker','image','inspect',IMAGE,
                                               '--format','{{json .RepoDigests}}'))
    command('docker', 'volume', 'create', '--label', 'elsebot.research=memory', VOLUME)
    volume_created = True
    command('docker','run','-d','--name',NAME,'--label','elsebot.research=memory',
            '--network','none','--memory','512m','--cpus','2',
            '-e','POSTGRES_HOST_AUTH_METHOD=trust','-v',VOLUME+':/var/lib/postgresql',IMAGE)
    container_created = True
    ready()
    RESULT['postgres_version'] = sql('SELECT version();')
    RESULT['settings'] = sql('SHOW synchronous_commit; SHOW fsync; SHOW server_encoding;')
    RESULT['port_bindings'] = command('docker','inspect',NAME,'--format','{{json .HostConfig.PortBindings}}')
    sql((ROOT/'schema.sql').read_text())
    sql("INSERT INTO memory_store(bot_id) VALUES ('facts'),('other'),('concurrent'),('tree');")
    RESULT['sql_output'] = sql((ROOT/'verify.sql').read_text())
    assert 'sql_checks_passed' in RESULT['sql_output']
    passed('sql_constraints_idempotency_supersession_and_search',
           assertions='immutability, foreign key, owner corrections, UTF-8 bytes, alignment, retry mismatch')
    with concurrent.futures.ThreadPoolExecutor(6) as pool:
        list(pool.map(worker, range(6)))
    counts = sql("SELECT count(*)||','||min(seq)||','||max(seq)||','||count(DISTINCT seq) "
                 "FROM memory_entry WHERE bot_id='concurrent';")
    assert counts == '121,0,120,121', counts
    assert sql("SELECT next_seq FROM memory_store WHERE bot_id='concurrent';") == '121'
    sql("BEGIN; SELECT fixture_append('concurrent','rolled-back','not committed'); ROLLBACK;")
    assert sql("SELECT next_seq FROM memory_store WHERE bot_id='concurrent';") == '121'
    passed('concurrent_append_and_rollback', workers=6, unique_entries=121,
           sequence_min=0, sequence_max=120, rolled_back_sequence_reused=True)
    lease_experiment()
    view_experiment()
    rollback_equivalence()
    # MVP delete is a tombstone. This models the authoritative bot record here.
    # Files and native sessions are outside this database fixture.
    sql("UPDATE memory_store SET deleted_at=now() WHERE bot_id='facts';")
    assert sql("SELECT count(*) FROM memory_entry WHERE bot_id='facts';") == '3'
    assert sql("SELECT count(*) FROM memory_job WHERE bot_id='facts';") == '3'
    assert sql("SELECT count(*) FROM memory_store WHERE bot_id='facts' AND deleted_at IS NULL;") == '0'
    try:
        sql("SELECT fixture_append('facts','after-delete','must not append');")
        raise AssertionError('tombstoned bot accepted an append')
    except RuntimeError as error:
        assert 'unknown bot' in str(error)
    passed('mvp_delete_tombstones_and_retains_memory', retained_entries=3,retained_jobs=3,
           note='Database-only check. VM files and native sessions are not exercised.')
    # Future explicit purge only. Never use this cascade for app-facing delete.
    sql("DELETE FROM memory_store WHERE bot_id='facts';")
    assert sql("SELECT count(*) FROM memory_entry WHERE bot_id='facts';") == '0'
    assert sql("SELECT count(*) FROM memory_job WHERE bot_id='facts';") == '0'
    assert sql("SELECT count(*) FROM memory_entry WHERE bot_id='concurrent';") == '121'
    passed('future_explicit_purge_one_bot_preserves_other_bot', remaining_other_bot_entries=121)
    RESULT['status'] = 'PASS'
except Exception as error:
    RESULT['status'] = 'FAIL'
    RESULT['error'] = str(error)
finally:
    if container_created:
        command('docker','rm','-f',NAME)
    if volume_created:
        command('docker','volume','rm',VOLUME)
    RESULT['cleanup'] = {'container_removed': container_created, 'volume_removed': volume_created}
    (ROOT/'results.json').write_text(json.dumps(RESULT,indent=2)+'\n')
    print(json.dumps(RESULT,indent=2))
if RESULT['status'] != 'PASS':
    raise SystemExit(1)
