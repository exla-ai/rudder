"""Retain ONE actual Child/pipes/pidfd through actual wait+EOF, even on error.
An expired original budget makes acceptance impossible. Settlement observation
may continue with those SAME objects; it never retries work or renews acceptance.
"""
import os,selectors,signal,subprocess,time
clock=lambda:time.clock_gettime(time.CLOCK_BOOTTIME)
MAX_LOG=16*1024*1024
HELD=[] # Even failed initialization publishes no success and retains this object.
def capture(argv,deadline,cwd=None,env=None,pass_fds=()):
 child=subprocess.Popen.__new__(subprocess.Popen);owner={'child':child,'pidfd':None,'selector':None,'streams':{},'stdout':bytearray(),'stderr':bytearray()};HELD.append(owner)
 eof={'stdout':False,'stderr':False};code=None;error=None;timed=False;term=False;kill=False;truncated=False;started=False
 try:
  # Retain fallible observer allocation BEFORE any actual child can exist.
  owner['selector']=selectors.DefaultSelector()
  if clock()>=deadline:raise RuntimeError('original-child-clock-expired-before-spawn')
  child.__init__(argv,cwd=cwd,env=env,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,pass_fds=pass_fds)
  started=True
 except BaseException as e:error=type(e).__name__
 # A partial __init__ may already own a real PID. Never discard that object.
 if getattr(child,'pid',None)is not None:
  try:owner['pidfd']=os.pidfd_open(child.pid,0)
  except BaseException as e:error=error or type(e).__name__
  sel=owner['selector']
  for name in eof:
   stream=getattr(child,name,None)
   if stream is not None:
    owner['streams'][name]=stream
    try:os.set_blocking(stream.fileno(),False);sel.register(stream,selectors.EVENT_READ,name)
    except BaseException as e:error=error or type(e).__name__
  while code is None or not all(eof.values()):
   now=clock();timed=timed or now>=deadline
   # Error/expiry ends useful work, never original handle custody. Same pidfd
   # and same monotonic deadline govern the fixed 10s kill tail.
   if (error or timed)and not term:
    term=True
    try:
     if owner['pidfd']is not None:signal.pidfd_send_signal(owner['pidfd'],signal.SIGTERM)
     else:child.terminate()
    except ProcessLookupError:pass
    except BaseException as e:error=error or type(e).__name__
   if (error or timed)and now>=deadline+10 and not kill:
    kill=True
    try:
     if owner['pidfd']is not None:signal.pidfd_send_signal(owner['pidfd'],signal.SIGKILL)
     else:child.kill()
    except ProcessLookupError:pass
    except BaseException as e:error=error or type(e).__name__
   try:
    for event,_ in sel.select(.05):
     try:part=os.read(event.fileobj.fileno(),65536)
     except BlockingIOError:continue
     name=event.data
     if not part:eof[name]=True;sel.unregister(event.fileobj);continue
     left=MAX_LOG-len(owner[name]);owner[name].extend(part[:max(0,left)]);truncated=truncated or len(part)>left
   except BaseException as e:error=error or type(e).__name__
   try:code=child.poll()
   except BaseException as e:error=error or type(e).__name__
  # Explicit actual wait, not poll/kill requested or is_finished as proof.
  while True:
   try:code=child.wait();break
   except BaseException as e:error=error or type(e).__name__
  # These SAME completed streams/pidfd/object remain in HELD for owner lifetime.
 return {'exitCode':code,'actualChildWait':getattr(child,'returncode',None)is not None,'stdoutEOF':eof['stdout'],'stderrEOF':eof['stderr'],'pidfdHeld':owner['pidfd']is not None,'timedOut':timed or clock()>=deadline,'truncated':truncated,'observerError':error,'constructorCompleted':started,'stdout':bytes(owner['stdout']),'stderr':bytes(owner['stderr'])}
