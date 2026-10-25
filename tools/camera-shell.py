import socket,time,sys
s=socket.create_connection(('192.168.1.103',2323),5); s.settimeout(.3)
def recv(t,marker=None):
 end=time.monotonic()+t; out=bytearray()
 while time.monotonic()<end:
  try: b=s.recv(65536)
  except socket.timeout: continue
  if not b: break
  i=0
  while i<len(b):
   if b[i]==255 and i+2<len(b) and b[i+1] in (251,252,253,254):
    if b[i+1] in (251,253): s.sendall(bytes([255,254 if b[i+1]==251 else 252,b[i+2]]))
    i+=3
   else: out.append(b[i]); i+=1
  if marker and marker in out: break
 return out.decode(errors='replace')
recv(1)
cmd=sys.stdin.read().strip()
s.sendall((cmd+'\nprintf "\\n__CAMERA_%s__\\n" FINISHED\n').encode())
print(recv(25,b'__CAMERA_FINISHED__'))
