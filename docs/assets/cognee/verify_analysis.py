"""Verify the document's numerical examples against pinned upstream pure functions.

No Cognee imports, model requests or database operations. Run from any directory.
"""
import argparse
import ast
import hashlib
import heapq
import json
import math
import re
import subprocess
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from uuid import NAMESPACE_OID, UUID, uuid5

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parents[2]
SHA = "663a2dc15d04bc0d7ec2733a2dd604b7ed1b8c8e"
parser = argparse.ArgumentParser()
parser.add_argument("--source", type=Path, default=REPO / ".local/research/cognee")
args = parser.parse_args()
SOURCE = args.source.resolve()
actual = subprocess.check_output(["git", "-C", str(SOURCE), "rev-parse", "HEAD"], text=True).strip()
assert actual == SHA, (actual, SHA)
checks = []
source_paths = set()


def functions(path, names, namespace, class_name=None):
    source_paths.add(path)
    tree = ast.parse((SOURCE / path).read_text(encoding="utf-8"))
    nodes = tree.body
    if class_name:
        nodes = next(n for n in nodes if isinstance(n, ast.ClassDef) and n.name == class_name).body
    picked = [n for n in nodes if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef)) and n.name in names]
    assert {n.name for n in picked} == set(names)
    module = ast.Module(body=[ast.ImportFrom(module="__future__", names=[ast.alias(name="annotations")], level=0), *picked], type_ignores=[])
    ast.fix_missing_locations(module)
    exec(compile(module, path, "exec"), namespace)
    return namespace


def close(a, b):
    assert math.isclose(a, b, rel_tol=1e-10, abs_tol=1e-10), (a, b)


ns = functions("cognee/modules/chunking/chunk_id.py", ["chunk_content_hash", "content_chunk_id"], dict(sha256=hashlib.sha256, uuid5=uuid5, NAMESPACE_OID=NAMESPACE_OID, UUID=UUID))
h = ns["chunk_content_hash"]("Atlas 服务由支付团队维护。")
cid = ns["content_chunk_id"]("document-a", h, 0)
assert cid == ns["content_chunk_id"]("document-a", h, 0)
assert cid != ns["content_chunk_id"]("document-a", h, 1)
assert cid != ns["content_chunk_id"]("document-b", h, 0)
checks.append(dict(name="content_chunk_identity", status="passed", same_text_same_scope_stable=True, occurrence_and_document_distinct=True))


class ValidationError(ValueError):
    def __init__(self, message, **kwargs):
        super().__init__(message)


ns = functions("cognee/tasks/memify/apply_feedback_weights.py", ["validate_feedback_alpha", "normalize_feedback_score", "stream_update_weight"], dict(CogneeValidationError=ValidationError, FEEDBACK_WEIGHT_DECIMALS=4))
weights = []
w = 0.5
for rating in [5, 5, 1]:
    w = ns["stream_update_weight"](w, ns["normalize_feedback_score"](rating), .1)
    weights.append(w)
assert weights == [.55, .595, .5355]
checks.append(dict(name="feedback_ema", status="passed", weights=weights))

ns = functions("cognee/modules/retrieval/hybrid/results.py", ["payload", "display_value", "result_id"], dict(Any=Any, UUID=UUID))
functions("cognee/modules/retrieval/hybrid/ranking.py", ["_rrf_k", "_importance_factor", "rank_chunk_summary_pairs"], ns)
pairs = [dict(chunk={"id": i, "importance_weight": importance}, chunk_id=i, vector_rank=vr, summary_rank=sr) for i,importance,vr,sr in [("A",.5,0,None),("B",.5,4,0),("C",0,1,2)]]
order = [p["chunk_id"] for p in ns["rank_chunk_summary_pairs"](pairs,5,True)]
assert order == ["B", "C", "A"], order
assert ns["_rrf_k"](5) == 30
scores = {p["chunk_id"]: sum(1/(30+r+1) for r in [p["vector_rank"],p["summary_rank"]] if r is not None) * ns["_importance_factor"](p["chunk"]) for p in pairs}
checks.append(dict(name="hybrid_rrf", status="passed", order=order, scores=scores))


class HeapCapture:
    def nsmallest(self, k, items, key):
        self.scores = {e.id: key(e) for e in items}
        return heapq.nsmallest(k, items, key=key)


capture = HeapCapture()
ns = functions("cognee/modules/graph/cognee_graph/CogneeGraph.py", ["_calculate_query_top_triplet_importances"], dict(heapq=capture, Any=Any), class_name="CogneeGraph")


def graph_score(beta, feedback, missing=False):
    def element(id_, d):
        return SimpleNamespace(id=id_, attributes=dict(vector_distance=[d],importance_weight=.5,feedback_weight=feedback))
    edge = element("edge",.3)
    edge.node1 = element("head",6.5 if missing else .2)
    edge.node2 = element("tail",.4)
    obj = SimpleNamespace(edges=[edge],feedback_influence=beta,personal_influence=0,triplet_distance_penalty=6.5)
    ns["_calculate_query_top_triplet_importances"](obj,1)
    return capture.scores["edge"]


graph_scores = [graph_score(0,.5), graph_score(.2,.5), graph_score(.2,.9), graph_score(.2,.9,True)]
for actual_score, expected in zip(graph_scores,[1.35,1.68,1.20,10.67]):
    close(actual_score,expected)
checks.append(dict(name="graph_triplet_scores", status="passed", scores=graph_scores, feedback_and_personal_scope="feedback tested; personal influence disabled"))

# Validate references in the new deliverables only. Historical documents are not rewritten.
docs = sorted((REPO / "docs").glob("Cognee_*.md")) + [ROOT / "README.md"]
link_count = 0
for doc in docs:
    content = doc.read_text(encoding="utf-8")
    assert content.count("```") % 2 == 0, f"unbalanced fence: {doc.name}"
    definitions = dict(re.findall(r"^\[([^\]]+)\]:\s+(\S+)",content,re.M))
    for ref in re.findall(r"\[[^\]\n]+\]\[([^\]]+)\]",content):
        assert ref in definitions, (doc.name,ref)
    urls = list(definitions.values()) + re.findall(r"!?\[[^\]]*\]\(([^)]+)\)",content)
    for url in urls:
        if "github.com/topoteretes/cognee/blob/" in url:
            prefix=f"https://github.com/topoteretes/cognee/blob/{SHA}/"
            assert url.startswith(prefix),url
            path=url[len(prefix):].split("#")[0]
            assert (SOURCE/path).is_file(),path
            source_paths.add(path)
        elif not re.match(r"[a-z]+://|#",url):
            path=(doc.parent/url.split("#")[0]).resolve()
            # The two reports are written by this script after validation.
            if path.name not in {"verification_report.json","source_manifest.json"}:
                assert path.exists(),f"broken link {doc.name}: {url}"
        link_count += 1
checks.append(dict(name="document_links_and_fences",status="passed",links_checked=link_count,documents=len(docs)))

files=[]
for path in sorted(source_paths):
    data=(SOURCE/path).read_bytes()
    files.append(dict(path=path,sha256=hashlib.sha256(data).hexdigest(),bytes=len(data),url=f"https://github.com/topoteretes/cognee/blob/{SHA}/{path}"))
manifest=dict(repository="https://github.com/topoteretes/cognee",commit=SHA,version="1.6.0",files=files)
(ROOT/"source_manifest.json").write_text(json.dumps(manifest,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
report=dict(source_commit=SHA,method="Static source review plus AST-extracted pure function checks. No Cognee package import or end-to-end execution.",checks=checks,referenced_source_files=len(files),limitations=["No database/LLM integration tests","No competitor accuracy, latency or cost benchmark","Optional truth/personal weighting not executed"])
(ROOT/"verification_report.json").write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
print(json.dumps(dict(checks=len(checks),status="passed",source_files=len(files),links_checked=link_count)))
