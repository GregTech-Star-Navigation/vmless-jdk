#include <windows.h>
#include <stdio.h>
__declspec(dllexport) int triple(int x) { return x * 3; }
BOOL WINAPI DllMain(HINSTANCE h, DWORD r, LPVOID p) { return TRUE; }
