import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import measure as M

p, ready = M.start("twosession.log")
try:
    print("time to ready %.3f" % ready)
    a = M.authorise()
    M.timed("s1 viewport layers=[]", lambda: M.viewport(a, []))
    M.timed("s1 viewport layers=[]", lambda: M.viewport(a, []))
    M.timed("s1 viewport kmeans", lambda: M.viewport(a, ["clusters/kmeans"]))
    M.timed("s1 viewport kmeans", lambda: M.viewport(a, ["clusters/kmeans"]))
    b = M.authorise()
    M.timed("s2 viewport layers=[]", lambda: M.viewport(b, []))
    M.timed("s2 viewport kmeans", lambda: M.viewport(b, ["clusters/kmeans"]))
    M.timed("s2 viewport kmeans", lambda: M.viewport(b, ["clusters/kmeans"]))
    M.timed("s1 viewport kmeans zoom3", lambda: M.viewport(a, ["clusters/kmeans"], zoom=3))
finally:
    M.stop(p)
