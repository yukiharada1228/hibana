export function isDate(value) {
  return Object.prototype.toString.call(value) === "[object Date]";
}
