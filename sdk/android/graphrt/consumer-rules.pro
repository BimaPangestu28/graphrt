# R8 / ProGuard rules for apps that use Graphrt: JNA and the UniFFI binding are
# called by name from native code.
-dontwarn java.awt.**
-keep class com.sun.jna.** { *; }
-keepclassmembers class * extends com.sun.jna.** { public *; }
-keep class uniffi.graphrt_ffi.** { *; }
