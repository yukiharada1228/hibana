import copy
import unittest
from results import summarize_resources


class ResourceEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.rows = [{
            'at': f'2026-09-08T00:00:{second:02d}+00:00',
            'workers': [{'uid': name, 'name': name, 'restarts': 0,
                         'memory_bytes': 1000 + second, 'cache_kib': 100}
                        for name in ('a', 'b')],
            'database': {'bytes': 10000, 'connections': 20, 'pending': 0,
                         'running': 1, 'executions': second},
        } for second in (0, 30)]

    def test_summarizes_observed_resources(self):
        report = summarize_resources(self.rows)
        self.assertEqual(report['worker_replacements'], 0)
        self.assertEqual(report['workers'][0]['memory_first_bytes'], 1000)
        self.assertEqual(report['workers'][0]['memory_last_bytes'], 1030)
        self.assertEqual(report['database']['max_running'], 1)

    def test_cannot_claim_success_after_restart_replacement_or_missing_pod(self):
        for mutation in ('restart', 'replacement', 'missing', 'duplicate'):
            with self.subTest(mutation=mutation):
                rows = copy.deepcopy(self.rows)
                workers = rows[1]['workers']
                if mutation == 'restart':
                    workers[0]['restarts'] = 1
                elif mutation == 'replacement':
                    workers[0]['uid'] = 'replacement'
                elif mutation == 'missing':
                    workers.pop()
                else:
                    workers[0]['uid'] = workers[1]['uid']
                with self.assertRaises(ValueError):
                    summarize_resources(rows)

    def test_missing_or_incomplete_observations_fail(self):
        with self.assertRaises(ValueError):
            summarize_resources(self.rows, minimum_seconds=7200)
        for timestamp in ('2026-09-08T00:02:00+00:00', self.rows[0]['at']):
            rows = copy.deepcopy(self.rows)
            rows[1]['at'] = timestamp
            with self.assertRaises(ValueError):
                summarize_resources(rows)
        self.rows[1]['observation_error'] = 'TimeoutError'
        with self.assertRaises(ValueError):
            summarize_resources(self.rows)


if __name__ == '__main__':
    unittest.main()
