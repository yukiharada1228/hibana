"""Summarize non-secret acceptance observations; reject gaps and replaced Pods."""
from datetime import datetime


def summarize_resources(rows, *, expected_workers=2, minimum_seconds=0):
    if len(rows) < 2:
        raise ValueError('Resource observations are missing')
    previous = None
    identities = None
    workers = {}
    for row in rows:
        if 'observation_error' in row:
            raise ValueError('Resource observation failed')
        at = datetime.fromisoformat(row['at'])
        if previous is not None and not 0 < (at - previous).total_seconds() <= 90:
            raise ValueError('Resource observation gap exceeds 90 seconds')
        previous = at
        sample = row['workers']
        current = {worker['uid'] for worker in sample}
        if len(sample) != expected_workers or len(current) != expected_workers:
            raise ValueError('Expected Worker Pods are missing')
        if identities is not None and current != identities:
            raise ValueError('Worker Pod was replaced during load')
        identities = current
        for worker in sample:
            if worker['restarts'] != 0:
                raise ValueError('Worker restarted during load')
            memory, cache = worker['memory_bytes'], worker['cache_kib']
            if memory <= 0 or cache < 0:
                raise ValueError('Invalid Worker resource values')
            item = workers.setdefault(worker['uid'], {
                'name': worker['name'], 'memory_first_bytes': memory,
                'memory_min_bytes': memory, 'memory_max_bytes': memory,
                'cache_first_kib': cache, 'cache_max_kib': cache,
            })
            item.update(memory_min_bytes=min(item['memory_min_bytes'], memory),
                        memory_max_bytes=max(item['memory_max_bytes'], memory),
                        memory_last_bytes=memory, cache_max_kib=max(item['cache_max_kib'], cache),
                        cache_last_kib=cache)
        database = row['database']
        if any(database[key] < 0 for key in ('bytes', 'connections', 'pending', 'running', 'executions')):
            raise ValueError('Invalid database observation')
    observed_seconds = (datetime.fromisoformat(rows[-1]['at']) - datetime.fromisoformat(rows[0]['at'])).total_seconds()
    if observed_seconds < minimum_seconds - 90:
        raise ValueError('Resource observations do not cover the requested duration')
    return {
        'samples': len(rows), 'observed_seconds': observed_seconds,
        'workers': list(workers.values()), 'worker_restarts': 0, 'worker_replacements': 0,
        'database': {
            'first': rows[0]['database'], 'last': rows[-1]['database'],
            'max_connections': max(row['database']['connections'] for row in rows),
            'max_pending': max(row['database']['pending'] for row in rows),
            'max_running': max(row['database']['running'] for row in rows),
        },
    }
