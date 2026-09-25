# JNI 冒烟（w11-e 复跑）：绕开命令直传时 -D 参数被终端解析层拆坏的坑
$javaArgs = @(
    '-Djava.library.path=target\release'
    '-cp', 'target\java-classes'
    'im.sdk.Demo'
)
& java @javaArgs
exit $LASTEXITCODE
