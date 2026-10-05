#!/usr/bin/env python3
"""Offline capture wiring fixtures; not compiler output or performance data."""
import importlib.util
import os
from pathlib import Path
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'tools/perf'))


class CodegenCaptureTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        path = ROOT / 'tools/perf/codegen_capture.py'
        if not path.is_file():
            raise AssertionError('real codegen capture utility is missing')
        spec = importlib.util.spec_from_file_location('codegen_capture', path)
        if spec is None or spec.loader is None:
            raise AssertionError('capture module cannot be loaded')
        cls.capture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.capture)
        driver_spec = importlib.util.spec_from_file_location('codegen_ci', ROOT / 'tools/perf/codegen_ci.py')
        if driver_spec is None or driver_spec.loader is None:
            raise AssertionError('capture driver cannot be loaded')
        cls.driver = importlib.util.module_from_spec(driver_spec)
        driver_spec.loader.exec_module(cls.driver)

    def test_encoded_flags_keep_precedence_and_save_temps(self):
        before = {'CARGO_ENCODED_RUSTFLAGS': '-C\x1ftarget-cpu=generic',
                  'RUSTFLAGS': '-C opt-level=1', 'CFLAGS': '-DMY_FLAG=7'}
        after = self.capture.diagnostic_env(before)
        self.assertEqual(before['RUSTFLAGS'], after['RUSTFLAGS'])
        self.assertEqual(after['CARGO_ENCODED_RUSTFLAGS'],
                         '-C\x1ftarget-cpu=generic\x1f-C\x1fsave-temps=yes')
        self.assertEqual(after['CFLAGS'], '-DMY_FLAG=7 -save-temps=obj')
        self.assertEqual(after['CC_ENABLE_DEBUG_OUTPUT'], '1')
        self.assertNotIn('CC_ENABLE_DEBUG_OUTPUT', before)

    def test_plain_flags_are_appended_not_replaced(self):
        after = self.capture.diagnostic_env({'RUSTFLAGS': '-C panic=unwind'})
        self.assertEqual(after['RUSTFLAGS'], '-C panic=unwind -C save-temps=yes')

    def test_final_stage_selection_retains_all_consumer_names(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get('TMPDIR')) as tmp:
            root = Path(tmp)
            names = ['ruzstd-x.cgu.0.rcgu.no-opt.bc',
                     'ruzstd-x.cgu.0.rcgu.thin-lto-input.bc',
                     'ruzstd-x.cgu.0.rcgu.bc',
                     'ruzstd-x.cgu.0.rcgu.thin-lto-after-pm.bc',
                     'ruzstd_perf_harness.cgu.0.rcgu.bc',
                     'ruzstd_perf_harness-x.cgu.1.rcgu.thin-lto-after-pm.bc',
                     'serde.cgu.0.rcgu.bc']
            for name in names:
                (root / name).touch()
            selected = self.capture.final_modules(root)
            self.assertEqual({p.name for p, _ in selected}, {
                names[3], names[4], names[5]})
            self.assertEqual({stage for _, stage in selected},
                             {'thin-lto-after-pm', 'optimized-rcgu'})

    def test_missing_consumer_or_final_stage_is_fatal(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get('TMPDIR')) as tmp:
            root = Path(tmp)
            (root / 'ruzstd-x.cgu.0.rcgu.no-opt.bc').touch()
            with self.assertRaises(ValueError):
                self.capture.final_modules(root)
            (root / 'ruzstd-x.cgu.0.rcgu.thin-lto-after-pm.bc').touch()
            with self.assertRaises(ValueError):
                self.capture.final_modules(root)

    def test_calibration_never_turns_signals_into_optimization_approval(self):
        codegen_ci = self.driver
        for state in ('GAIN', 'REGRESSION', 'TRADEOFF', 'UNRESOLVED'):
            report = {'data_status': 'COMPLETE', 'accepted': True,
                      'cases': [{'performance_status': state}]}
            self.assertEqual(codegen_ci.calibration_status('same-binary-aa', report), 'UNRESOLVED')
            self.assertEqual(codegen_ci.calibration_status('independent-build-aa', report), 'UNRESOLVED')
        self.assertEqual(codegen_ci.calibration_status('same-binary-aa', {}), 'INCOMPLETE')
        self.assertEqual(codegen_ci.calibration_status('same-binary-aa', {
            'data_status': 'COMPLETE', 'cases': [{'performance_status': 'NO_CHANGE'}]}),
                         'NO_SIGNAL_IN_THIS_BUDGET')
        self.assertIsNone(codegen_ci.calibration_status('candidate-pr50', {}))

    def test_codegen_workflow_preserves_three_failure_domains_and_normal_link(self):
        import yaml
        data = yaml.safe_load((ROOT / '.github/workflows/codegen.yml').read_text())
        self.assertIn('workflow_dispatch', data['on'])
        self.assertEqual(data['on']['push']['branches'], ['test/llvm-codegen-calibration'])
        steps = data['jobs']['capture']['steps']
        script = '\n'.join(step.get('run', '') for step in steps)
        self.assertIn('codegen_ci.py', script)
        upload = next(s for s in steps if s.get('uses', '').startswith('actions/upload-artifact'))
        self.assertEqual(upload['if'], 'always()')
        driver = (ROOT / 'tools/perf/codegen_ci.py').read_text()
        self.assertIn("'same-binary-aa'", driver)
        self.assertIn("'independent-build-aa'", driver)
        self.assertIn("'candidate-pr50'", driver)
        self.assertIn("'optimization_approved': False", driver)
        self.assertIn("'--require-execution'", driver)
        self.assertNotIn('--emit=llvm-ir', driver)

    def test_call_sites_do_not_count_comments_labels_or_black_box(self):
        ir = '''define void @f() {
entry:
; call void @fake()
bb.invoke:
  br label %bb.invoke2
bb.invoke2:
  call void asm sideeffect "", "~{memory}"()
  call void @real()
  %x = invoke i64 @other() to label %end unwind label %cleanup
end:
  ret void
}'''
        self.assertEqual(self.capture.direct_calls(ir), ['real', 'other'])


if __name__ == '__main__':
    unittest.main(verbosity=2)
