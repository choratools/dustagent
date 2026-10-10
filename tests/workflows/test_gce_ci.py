"""Offline admission contract; run with uv --with PyYAML."""
from pathlib import Path
import re
import unittest
import yaml

ROOT = Path(__file__).resolve().parents[2]

class GceCiTests(unittest.TestCase):
    def test_ci_is_read_only_main_only_and_same_repository(self):
        text = (ROOT / '.github/workflows/ci.yml').read_text()
        workflow = yaml.load(text, Loader=yaml.BaseLoader)
        self.assertEqual(workflow['permissions'], {'contents': 'read'})
        self.assertEqual(workflow['on'], {'push': {'branches': ['main']}, 'pull_request': {'branches': ['main']}})
        job = workflow['jobs']['check-and-test']
        self.assertEqual(job['runs-on'], ['self-hosted', 'Linux', 'X64', 'route-one', 'r1-run-${{ github.run_id }}-check-and-test'])
        self.assertIn('github.event.pull_request.head.repo.full_name == github.repository', job['if'])
        self.assertNotIn('environment', job)
        self.assertNotIn('id-token', text)
        self.assertNotIn('secrets', text)
        self.assertEqual(job['steps'][0]['with']['persist-credentials'], 'false')
        for step in job['steps']:
            if 'uses' in step:
                self.assertRegex(step['uses'], r'^[^@]+@[a-f0-9]{40}$')
        rust = job['steps'][1]
        self.assertTrue(rust['uses'].startswith('dtolnay/rust-toolchain@'))
        self.assertEqual(rust['with']['components'], 'rustfmt, clippy')
