// https://github.com/LadybirdBrowser/ladybird/issues/3584
// Converting an object with a deep ordinary prototype chain walks the chain iteratively.
test("converting an object with a very deep prototype chain to a primitive does not crash", () => {
    // Object.create() sets the prototype at creation without a cycle check — so the chain is built in linear time.
    let object = {};
    for (let i = 0; i < 1_000_000; ++i) object = Object.create(object);

    expect(Number(object)).toBeNaN();
});
