import os, sys, time, requests, json
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import measure as M

def vp(tok, layers, computed=None):
    body = {"view":"knn","zoom":0,"bbox":M.BBOX,"k":0,"layers":layers}
    if computed is not None: body["computed"]=computed
    r=requests.post(M.VIEWER+"/v1/viewport",headers={"Authorization":"Bearer "+tok},json=body,timeout=900)
    if r.status_code!=200: raise RuntimeError("%s %r"%(r.status_code,r.content[:300]))
    return len(r.content)

p, ready = M.start("computed.log")
try:
    print("time to ready %.3f" % ready)
    for computed in ([], ["centroid"], ["centroid","box"], ["shape"], None):
        tok = M.authorise()
        M.timed("fresh session, kmeans computed=%s" % computed, lambda c=computed: vp(tok, ["clusters/kmeans"], c))
        M.timed("  same session again", lambda c=computed: vp(tok, ["clusters/kmeans"], c))
    tok = M.authorise()
    M.timed("fresh session, mesh (row-major)", lambda: vp(tok, ["mesh/descriptors"], []))
    M.timed("  same session again", lambda: vp(tok, ["mesh/descriptors"], []))
finally:
    M.stop(p)
