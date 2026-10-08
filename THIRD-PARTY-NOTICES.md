# Third-party notices for Agora

## work.py (g-cqd/skills, planner skill)

`crates/agora-sched` and `crates/agora-run` port the scheduling logic of `work.py`, the task queue and
capacity tool in the planner skill of g-cqd/skills: the ordering rules, the admission decision, and the
supervisor behaviour. The test fixtures in `crates/agora-sched/tests/fixtures/ordering/` were generated
from `work.py`.

The MIT License below is the license of the g-cqd/skills repository, from its `LICENSE` file. The LICENSE's paragraph about portions derived from other projects is not reproduced, because it does not concern work.py.

```text
MIT License

Copyright (c) 2026 g-cqd

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```
